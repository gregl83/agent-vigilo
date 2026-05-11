//! RabbitMQ client wrapper used by coordinators and workers.
//!
//! The client lazily opens one connection and one channel per process context,
//! declares the durable topic exchange on first use, and provides the small set
//! of operations the runtime needs: publish JSON events, fetch one worker
//! message, and acknowledge or requeue deliveries.

use lapin::{
    BasicProperties,
    Channel,
    Connection,
    ConnectionProperties,
    ExchangeKind,
    options::{
        BasicAckOptions,
        BasicGetOptions,
        BasicNackOptions,
        BasicPublishOptions,
        ExchangeDeclareOptions,
        QueueBindOptions,
        QueueDeclareOptions,
    },
    types::FieldTable,
};
use serde_json::Value;
use tokio::sync::OnceCell;
use tracing::debug;

/// RabbitMQ connection and routing configuration.
///
/// The exchange is a durable topic exchange used for all runtime events. The
/// worker queue receives `run.chunk.ready` events consumed by worker commands.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) uri: String,
    pub(crate) exchange: String,
    pub(crate) worker_queue: String,
}

impl Config {
    /// Builds default queue/exchange names around a caller-provided broker URI.
    pub(crate) fn new(uri: String) -> Self {
        Self {
            uri,
            exchange: "vigilo.events".to_string(),
            worker_queue: "vigilo.worker".to_string(),
        }
    }
}

/// A JSON message fetched from the worker queue.
///
/// The `delivery_tag` must be passed back to `ack` or `nack_requeue` after the
/// caller has processed the payload.
pub(crate) struct ConsumedMessage {
    pub(crate) delivery_tag: u64,
    pub(crate) payload: Value,
}

/// Lazily initialized RabbitMQ connection and channel.
///
/// The client owns connection setup and broker declarations so command code can
/// work in terms of application events instead of `lapin` primitives.
pub(crate) struct Client {
    config: Config,
    connection: OnceCell<Connection>,
    channel: OnceCell<Channel>,
}

impl Client {
    /// Creates a client shell; no network connection is opened until first use.
    pub(crate) fn new(config: Config) -> Self {
        Self {
            config,
            connection: OnceCell::new(),
            channel: OnceCell::new(),
        }
    }

    /// Returns the process-local RabbitMQ connection, opening it if needed.
    async fn connection(&self) -> anyhow::Result<&Connection> {
        self.connection
            .get_or_try_init(|| async {
                debug!("initializing rabbitmq connection");
                Connection::connect(&self.config.uri, ConnectionProperties::default())
                    .await
                    .map_err(|err| anyhow::anyhow!("rabbitmq connection failed: {}", err))
            })
            .await
    }

    /// Returns the process-local channel and ensures the topic exchange exists.
    async fn channel(&self) -> anyhow::Result<&Channel> {
        self.channel
            .get_or_try_init(|| async {
                let connection = self.connection().await?;
                let channel = connection
                    .create_channel()
                    .await
                    .map_err(|err| anyhow::anyhow!("rabbitmq channel creation failed: {}", err))?;

                channel
                    .exchange_declare(
                        &self.config.exchange,
                        ExchangeKind::Topic,
                        ExchangeDeclareOptions {
                            durable: true,
                            auto_delete: false,
                            internal: false,
                            nowait: false,
                            passive: false,
                        },
                        FieldTable::default(),
                    )
                    .await
                    .map_err(|err| {
                        anyhow::anyhow!("rabbitmq exchange declaration failed: {}", err)
                    })?;

                Ok(channel)
            })
            .await
    }

    /// Publishes a JSON payload to the configured topic exchange.
    pub(crate) async fn publish_json(
        &self,
        routing_key: &str,
        payload: &Value,
    ) -> anyhow::Result<()> {
        let body = serde_json::to_vec(payload)
            .map_err(|err| anyhow::anyhow!("failed to serialize message payload: {}", err))?;

        let channel = self.channel().await?;
        channel
            .basic_publish(
                &self.config.exchange,
                routing_key,
                BasicPublishOptions::default(),
                &body,
                BasicProperties::default().with_content_type("application/json".into()),
            )
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq publish failed: {}", err))?
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq publish confirmation failed: {}", err))?;

        Ok(())
    }

    /// Fetches one available worker message without creating a long-lived consumer.
    ///
    /// Workers currently poll with `basic_get`, which keeps the command model
    /// simple for one-shot and looped worker modes. The queue binding is
    /// idempotently declared before each fetch.
    pub(crate) async fn consume_worker_message(&self) -> anyhow::Result<Option<ConsumedMessage>> {
        let channel = self.channel().await?;

        channel
            .queue_declare(
                &self.config.worker_queue,
                QueueDeclareOptions {
                    passive: false,
                    durable: true,
                    exclusive: false,
                    auto_delete: false,
                    nowait: false,
                },
                FieldTable::default(),
            )
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq queue declaration failed: {}", err))?;

        channel
            .queue_bind(
                &self.config.worker_queue,
                &self.config.exchange,
                "run.chunk.ready",
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq queue binding failed: {}", err))?;

        let maybe_delivery = channel
            .basic_get(&self.config.worker_queue, BasicGetOptions::default())
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq consume failed: {}", err))?;

        let Some(delivery) = maybe_delivery else {
            return Ok(None);
        };

        let payload = serde_json::from_slice::<Value>(&delivery.data)
            .map_err(|err| anyhow::anyhow!("failed to deserialize message payload: {}", err))?;

        Ok(Some(ConsumedMessage {
            delivery_tag: delivery.delivery_tag,
            payload,
        }))
    }

    /// Acknowledges successful processing of one delivery.
    pub(crate) async fn ack(&self, delivery_tag: u64) -> anyhow::Result<()> {
        let channel = self.channel().await?;
        channel
            .basic_ack(delivery_tag, BasicAckOptions::default())
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq ack failed: {}", err))?;
        Ok(())
    }

    /// Rejects one delivery and asks RabbitMQ to make it available again.
    pub(crate) async fn nack_requeue(&self, delivery_tag: u64) -> anyhow::Result<()> {
        let channel = self.channel().await?;
        channel
            .basic_nack(
                delivery_tag,
                BasicNackOptions {
                    multiple: false,
                    requeue: true,
                },
            )
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq nack failed: {}", err))?;
        Ok(())
    }
}
