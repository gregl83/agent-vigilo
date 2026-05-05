use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunChunkDraft {
    pub(crate) chunk_id: Uuid,
    pub(crate) profile_group_id: String,
    pub(crate) ordinal_start: i32,
    pub(crate) ordinal_end: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct RunChunk {
    pub(crate) id: Uuid,
    pub(crate) run_id: Uuid,
    pub(crate) dataset_version_id: String,
    pub(crate) profile_group_id: String,
    pub(crate) ordinal_start: i32,
    pub(crate) ordinal_end: i32,
    pub(crate) status: String,
    pub(crate) leased_until: Option<DateTime<Utc>>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

