//! Shard placement persistence models.
//!
//! Shard placements map one `run_id + run_shard` pair to a configured database
//! placement alias. They are control-database routing metadata, not evaluator
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

pub(crate) const SHARD_PLACEMENT_STATUS_ACTIVE: &str = "active";
pub(crate) const SHARD_PLACEMENT_STATUS_COPYING: &str = "copying";
pub(crate) const SHARD_PLACEMENT_STATUS_MOVING: &str = "moving";
pub(crate) const SHARD_PLACEMENT_STATUS_DRAINING: &str = "draining";

/// Insert payload for a shard placement row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ShardPlacementDraft {
    /// Run whose logical shard should be routed.
    pub(crate) run_id: Uuid,
    /// Logical shard number inside the 128-shard range.
    pub(crate) run_shard: i16,
    /// Database placement alias for this run shard.
    pub(crate) database_alias: String,
    /// Placement lifecycle: active, copying, draining, or moving.
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
    /// Placement lifecycle: active, copying, draining, or moving.
    pub(crate) status: String,
    /// Durable destination reservation while a shard is copying, draining, or moving.
    pub(crate) move_target_database_alias: Option<String>,
    /// Monotonic route fencing token.
    pub(crate) route_version: i64,
    /// Monotonic execution write-ownership generation.
    pub(crate) write_epoch: i64,
    /// Time this placement row was inserted.
    pub(crate) created_at: DateTime<Utc>,
    /// Time this placement row was last updated.
    pub(crate) updated_at: DateTime<Utc>,
}

impl ShardPlacement {
    pub(crate) fn is_dispatchable(&self) -> bool {
        matches!(
            self.status.as_str(),
            SHARD_PLACEMENT_STATUS_ACTIVE | SHARD_PLACEMENT_STATUS_COPYING
        )
    }

    pub(crate) fn same_route_fence(&self, current: &Self) -> bool {
        self.database_alias == current.database_alias
            && self.status == current.status
            && self.route_version == current.route_version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(status: &str) -> ShardPlacement {
        let now = Utc::now();
        ShardPlacement {
            run_id: Uuid::now_v7(),
            run_shard: 4,
            database_alias: "source".to_string(),
            status: status.to_string(),
            move_target_database_alias: Some("target".to_string()),
            route_version: 2,
            write_epoch: 1,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn copying_route_remains_dispatchable_during_online_backfill() {
        assert!(placement(SHARD_PLACEMENT_STATUS_COPYING).is_dispatchable());
    }

    #[test]
    fn draining_and_moving_routes_reject_new_dispatch() {
        assert!(!placement(SHARD_PLACEMENT_STATUS_DRAINING).is_dispatchable());
        assert!(!placement(SHARD_PLACEMENT_STATUS_MOVING).is_dispatchable());
    }
}
