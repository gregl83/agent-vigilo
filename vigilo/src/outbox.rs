//! Durable outbox publication.
//!
//! Database workflows insert events into `outbox_events` in the same
//! transaction as state changes. The coordinator calls this module after each
//! orchestration pass to claim a bounded batch, publish each event to the
//! message broker, and either mark it published or reschedule it for retry.
//! This keeps database state and external message delivery loosely coupled
//! without losing events when a process exits between commit and publish.

use async_trait::async_trait;
use sqlx::PgPool;
use tracing::error;

use crate::{
    db::tables::outbox_events,
    models::outbox_event::OutboxEvent,
    mq,
};

/// Runtime knobs for one outbox publishing pass.
///
/// `batch_size` bounds coordinator work per cycle, `lease_seconds` prevents
/// other coordinators from claiming the same events immediately, and
/// `retry_delay_seconds` controls when a failed publish becomes eligible again.
#[derive(Debug, Clone)]
pub(crate) struct OutboxPublisherConfig {
    pub(crate) batch_size: i64,
    pub(crate) lease_seconds: i32,
    pub(crate) retry_delay_seconds: i32,
}

impl Default for OutboxPublisherConfig {
    fn default() -> Self {
        Self {
            batch_size: 32,
            lease_seconds: 30,
            retry_delay_seconds: 10,
        }
    }
}

/// Counts produced by a single publish pass.
///
/// `claimed` is the number of events leased from the database. `published` and
/// `failed` describe the outcomes for those claimed events.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct OutboxPublishStats {
    pub(crate) claimed: usize,
    pub(crate) published: usize,
    pub(crate) failed: usize,
}

/// Transport boundary for publishing outbox events.
///
/// Keeping this as a trait lets the coordinator publish to RabbitMQ in
/// production while tests or future transports can supply a different
/// implementation without changing claim/reschedule behavior.
#[async_trait]
pub(crate) trait EventPublisher: Send + Sync {
    async fn publish(&self, event: &OutboxEvent) -> anyhow::Result<()>;
}

/// RabbitMQ-backed event publisher.
pub(crate) struct MqEventPublisher<'a> {
    client: &'a mq::Client,
}

impl<'a> MqEventPublisher<'a> {
    /// Wraps the shared message-queue client without taking ownership.
    pub(crate) fn new(client: &'a mq::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl EventPublisher for MqEventPublisher<'_> {
    async fn publish(&self, event: &OutboxEvent) -> anyhow::Result<()> {
        self.client
            .publish_json(&event.event_type, &event.payload)
            .await
    }
}

/// Claims and publishes a bounded batch of pending outbox events.
///
/// Each claimed event is published independently. Successful publishes are
/// marked `published`; failures are logged and rescheduled so a later
/// coordinator pass can retry them.
pub(crate) async fn publish_pending_events(
    db: &PgPool,
    publisher: &dyn EventPublisher,
    config: &OutboxPublisherConfig,
) -> anyhow::Result<OutboxPublishStats> {
    let claimed_events =
        outbox_events::claim_publishable_outbox_events(db, config.batch_size, config.lease_seconds)
            .await?;

    let mut stats = OutboxPublishStats {
        claimed: claimed_events.len(),
        ..OutboxPublishStats::default()
    };

    for event in claimed_events {
        match publisher.publish(&event).await {
            Ok(()) => {
                outbox_events::mark_outbox_event_published(db, event.id).await?;
                stats.published += 1;
            }
            Err(err) => {
                let message = err.to_string();
                error!(
                    event_id = %event.id,
                    event_type = %event.event_type,
                    error = %message,
                    "outbox publish failed; scheduling retry"
                );
                outbox_events::reschedule_outbox_event(
                    db,
                    event.id,
                    config.retry_delay_seconds,
                    &message,
                )
                .await?;
                stats.failed += 1;
            }
        }
    }

    Ok(stats)
}
