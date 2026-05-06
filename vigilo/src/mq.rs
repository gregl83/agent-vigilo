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

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) uri: String,
    pub(crate) exchange: String,
    pub(crate) worker_queue: String,
}

impl Config {
    pub(crate) fn new(uri: String) -> Self {
        Self {
            uri,
            exchange: "vigilo.events".to_string(),
            worker_queue: "vigilo.worker".to_string(),
        }
    }
}

pub(crate) struct ConsumedMessage {
    pub(crate) delivery_tag: u64,
    pub(crate) payload: Value,
}

pub(crate) struct Client {
    config: Config,
    connection: OnceCell<Connection>,
    channel: OnceCell<Channel>,
}

impl Client {
    pub(crate) fn new(config: Config) -> Self {
        Self {
            config,
            connection: OnceCell::new(),
            channel: OnceCell::new(),
        }
    }

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

    pub(crate) async fn ack(&self, delivery_tag: u64) -> anyhow::Result<()> {
        let channel = self.channel().await?;
        channel
            .basic_ack(delivery_tag, BasicAckOptions::default())
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq ack failed: {}", err))?;
        Ok(())
    }

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
