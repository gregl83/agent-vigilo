use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OutboxEventDraft {
    pub(crate) event_type: String,
    pub(crate) aggregate_type: String,
    pub(crate) aggregate_id: Uuid,
    pub(crate) dedupe_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OutboxEventPatch {
    pub(crate) status: String,
    pub(crate) error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct OutboxEvent {
    pub(crate) id: Uuid,
    pub(crate) event_type: String,
    pub(crate) aggregate_type: String,
    pub(crate) aggregate_id: Uuid,
    pub(crate) dedupe_key: String,
    pub(crate) payload: serde_json::Value,
    pub(crate) status: String,
    pub(crate) available_at: DateTime<Utc>,
    pub(crate) published_at: Option<DateTime<Utc>>,
    pub(crate) error_message: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}
