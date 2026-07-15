//! Database placement persistence models.
//!
//! Database placements are the routing catalog for future multi-database
//! deployments. The default deployment seeds one `primary` placement that uses
//! `DATABASE_URL` for both control-plane and shard-local data.

use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};

pub(crate) const DEFAULT_DATABASE_ALIAS: &str = "primary";
pub(crate) const DEFAULT_DATABASE_URL_ENV: &str = "DATABASE_URL";

pub(crate) const DATABASE_PLACEMENT_ROLE_CONTROL: &str = "control";
pub(crate) const DATABASE_PLACEMENT_ROLE_SHARD: &str = "shard";
pub(crate) const DATABASE_PLACEMENT_ROLE_CONTROL_AND_SHARD: &str = "control_and_shard";
pub(crate) const DATABASE_PLACEMENT_STATUS_ACTIVE: &str = "active";
pub(crate) const DATABASE_PLACEMENT_STATUS_DISABLED: &str = "disabled";

/// Persisted database placement row.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct DatabasePlacement {
    /// Stable routing alias.
    pub(crate) alias: String,
    /// Environment variable that contains this placement's database URL.
    pub(crate) database_url_env: String,
    /// Placement role: control, shard, or control_and_shard.
    pub(crate) role: String,
    /// Placement status: active or disabled.
    pub(crate) status: String,
    /// Time this placement row was inserted.
    pub(crate) created_at: DateTime<Utc>,
    /// Time this placement row was last updated.
    pub(crate) updated_at: DateTime<Utc>,
}

impl DatabasePlacement {
    pub(crate) fn is_control_capable(&self) -> bool {
        matches!(
            self.role.as_str(),
            DATABASE_PLACEMENT_ROLE_CONTROL | DATABASE_PLACEMENT_ROLE_CONTROL_AND_SHARD
        )
    }

    pub(crate) fn is_shard_capable(&self) -> bool {
        matches!(
            self.role.as_str(),
            DATABASE_PLACEMENT_ROLE_SHARD | DATABASE_PLACEMENT_ROLE_CONTROL_AND_SHARD
        )
    }
}
