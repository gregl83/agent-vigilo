use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DatasetVersionCaseDraft {
    pub(crate) case_id: String,
    pub(crate) case_ordinal: i32,
    pub(crate) case_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct DatasetVersionCase {
    pub(crate) dataset_version_id: String,
    pub(crate) case_id: String,
    pub(crate) case_ordinal: i32,
    pub(crate) case_hash: String,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

