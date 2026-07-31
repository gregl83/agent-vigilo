//! Run-scoped case projection models.
//!
//! Each row assigns one immutable dataset case to the logical run shard that
//! may read it from an execution database.

use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

/// Insert payload for one shard-local case projection row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct RunShardCaseDraft {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) dataset_version_id: Uuid,
    pub(crate) case_id: Uuid,
    pub(crate) case_ordinal: i32,
    pub(crate) case_hash: String,
}
