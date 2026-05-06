use lapin::{
    BasicProperties,
    Channel,
    Connection,
    ConnectionProperties,
    ExchangeKind,
    options::{
        BasicPublishOptions,
        ExchangeDeclareOptions,
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
}

impl Config {
    pub(crate) fn new(uri: String) -> Self {
        Self {
            uri,
            exchange: "vigilo.events".to_string(),
        }
    }
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
                    .map_err(|err| anyhow::anyhow!("rabbitmq exchange declaration failed: {}", err))?;

                Ok(channel)
            })
            .await
    }

    pub(crate) async fn publish_json(&self, routing_key: &str, payload: &Value) -> anyhow::Result<()> {
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
}



