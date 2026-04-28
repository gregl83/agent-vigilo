use sqlx::types::JsonValue;

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct OutboxEvent {
    pub(crate) id: String,
    pub(crate) event_type: String,
    pub(crate) aggregate_type: String,
    pub(crate) aggregate_id: String,
    pub(crate) dedupe_key: String,
    pub(crate) payload: JsonValue,
    pub(crate) status: String,
    pub(crate) available_at: String,
    pub(crate) published_at: Option<String>,
    pub(crate) error_message: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Debug, Clone)]
pub(crate) struct NewOutboxEvent {
    pub(crate) event_type: String,
    pub(crate) aggregate_type: String,
    pub(crate) aggregate_id: String,
    pub(crate) dedupe_key: String,
}

