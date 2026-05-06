use async_trait::async_trait;
use sqlx::PgPool;
use tracing::{
    error,
    info,
};

use crate::{
    db::outbox_events,
    mq,
    models::outbox_event::OutboxEvent,
};

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

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct OutboxPublishStats {
    pub(crate) claimed: usize,
    pub(crate) published: usize,
    pub(crate) failed: usize,
}

#[async_trait]
pub(crate) trait EventPublisher: Send + Sync {
    async fn publish(&self, event: &OutboxEvent) -> anyhow::Result<()>;
}

#[derive(Debug, Default)]
pub(crate) struct LoggingEventPublisher;

pub(crate) struct MqEventPublisher<'a> {
    client: &'a mq::Client,
}

impl<'a> MqEventPublisher<'a> {
    pub(crate) fn new(client: &'a mq::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl EventPublisher for LoggingEventPublisher {
    async fn publish(&self, event: &OutboxEvent) -> anyhow::Result<()> {
        // Temporary publisher for coordinator-owned outbox dispatch; replace with MQ adapter later.
        info!(
            event_id = %event.id,
            event_type = %event.event_type,
            aggregate_type = %event.aggregate_type,
            aggregate_id = %event.aggregate_id,
            payload = %event.payload,
            "published outbox event"
        );

        Ok(())
    }
}

#[async_trait]
impl EventPublisher for MqEventPublisher<'_> {
    async fn publish(&self, event: &OutboxEvent) -> anyhow::Result<()> {
        self.client.publish_json(&event.event_type, &event.payload).await
    }
}

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
