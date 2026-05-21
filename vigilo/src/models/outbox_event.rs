//! Outbox event persistence models.
//!
//! Outbox events are durable messages written inside database transactions and
//! later published by the coordinator. The table records claim/retry state so
//! message publication can be retried without losing committed state changes.

use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

/// Insert payload for a durable outbox event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OutboxEventDraft {
    /// Routing key or semantic event type to publish.
    pub(crate) event_type: String,
    /// Aggregate category associated with the event, such as `run`.
    pub(crate) aggregate_type: String,
    /// Aggregate instance id associated with the event.
    pub(crate) aggregate_id: Uuid,
    /// Idempotency key used to avoid duplicate event rows.
    pub(crate) dedupe_key: String,
}

/// Mutable outbox delivery status fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OutboxEventPatch {
    /// New outbox lifecycle status.
    pub(crate) status: String,
    /// Latest delivery error, if any.
    pub(crate) error_message: Option<String>,
}

/// Persisted outbox event row.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct OutboxEvent {
    /// Event row id.
    pub(crate) id: Uuid,
    /// Routing key or semantic event type to publish.
    pub(crate) event_type: String,
    /// Aggregate category associated with the event, such as `run`.
    pub(crate) aggregate_type: String,
    /// Aggregate instance id associated with the event.
    pub(crate) aggregate_id: Uuid,
    /// Idempotency key used to avoid duplicate event rows.
    pub(crate) dedupe_key: String,
    /// JSON event body to publish.
    pub(crate) payload: serde_json::Value,
    /// Current outbox lifecycle status.
    pub(crate) status: String,
    /// Earliest time the event may be claimed for publication.
    pub(crate) available_at: DateTime<Utc>,
    /// Current publisher claim token, if the event is leased for publication.
    pub(crate) claim_token: Option<Uuid>,
    /// Deadline for the current publisher claim.
    pub(crate) claimed_until: Option<DateTime<Utc>>,
    /// Number of publication claims issued for this event.
    pub(crate) publish_attempt_count: i32,
    /// Time the event was successfully published.
    pub(crate) published_at: Option<DateTime<Utc>>,
    /// Latest delivery error, if any.
    pub(crate) error_message: Option<String>,
    /// Time this event row was inserted.
    pub(crate) created_at: DateTime<Utc>,
    /// Time this event row was last updated.
    pub(crate) updated_at: DateTime<Utc>,
}
