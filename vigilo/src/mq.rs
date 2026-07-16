//! RabbitMQ client wrapper used by coordinators and workers.
//!
//! The client lazily opens a broker session with a publish channel, declares
//! topology for each fresh session, and recreates the session after connection
//! or channel loss. Long-lived worker consumers use their own channel while
//! delivery acknowledgements use the message's channel-scoped acker.

use std::sync::Arc;

use lapin::{
    BasicProperties,
    Channel,
    Connection,
    ConnectionProperties,
    Error as LapinError,
    ExchangeKind,
    acker::Acker,
    options::{
        BasicAckOptions,
        BasicConsumeOptions,
        BasicGetOptions,
        BasicPublishOptions,
        BasicQosOptions,
        ConfirmSelectOptions,
        ExchangeDeclareOptions,
        QueueBindOptions,
        QueueDeclareOptions,
    },
    publisher_confirm::Confirmation,
    types::{
        AMQPValue,
        FieldTable,
        LongString,
        ShortString,
    },
};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{
    debug,
    warn,
};
use uuid::Uuid;

const WORKER_ROUTING_KEY: &str = "run.chunk.ready";
const WORKER_MAX_RETRIES: i32 = 8;
const WORKER_RETRY_BUCKETS: [(&str, i32); 8] = [
    ("vigilo.worker.retry.5s", 5_000),
    ("vigilo.worker.retry.10s", 10_000),
    ("vigilo.worker.retry.20s", 20_000),
    ("vigilo.worker.retry.40s", 40_000),
    ("vigilo.worker.retry.80s", 80_000),
    ("vigilo.worker.retry.160s", 160_000),
    ("vigilo.worker.retry.320s", 320_000),
    ("vigilo.worker.retry.640s", 640_000),
];
const HEADER_RETRY_COUNT: &str = "x-vigilo-retry-count";
const HEADER_FIRST_FAILED_AT: &str = "x-vigilo-first-failed-at";
const HEADER_LAST_FAILED_AT: &str = "x-vigilo-last-failed-at";
const HEADER_LAST_ERROR: &str = "x-vigilo-last-error";
const HEADER_ERROR_CLASS: &str = "x-vigilo-error-class";
const HEADER_ORIGINAL_EXCHANGE: &str = "x-vigilo-original-exchange";
const HEADER_ORIGINAL_ROUTING_KEY: &str = "x-vigilo-original-routing-key";

/// RabbitMQ connection and routing configuration.
///
/// The exchange is a durable topic exchange used for all runtime events. The
/// worker queue receives `run.chunk.ready` events consumed by worker commands.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) uri: String,
    pub(crate) exchange: String,
    pub(crate) event_queue: String,
    pub(crate) worker_queue: String,
    pub(crate) worker_retry_exchange: String,
    pub(crate) worker_quarantine_exchange: String,
    pub(crate) worker_quarantine_queue: String,
}

impl Config {
    /// Builds default queue/exchange names around a caller-provided broker URI.
    ///
    /// Set `VIGILO_MQ_NAMESPACE` to isolate broker topology for integration
    /// tests or parallel local stacks. When unset, the historic queue and
    /// exchange names are preserved.
    pub(crate) fn new(uri: String) -> Self {
        let namespace = std::env::var("VIGILO_MQ_NAMESPACE")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        if let Some(namespace) = namespace {
            let prefix = format!("vigilo.{}", namespace);
            return Self {
                uri,
                exchange: format!("{prefix}.events"),
                event_queue: format!("{prefix}.events.domain"),
                worker_queue: format!("{prefix}.worker"),
                worker_retry_exchange: format!("{prefix}.worker.retry"),
                worker_quarantine_exchange: format!("{prefix}.worker.quarantine"),
                worker_quarantine_queue: format!("{prefix}.worker.quarantine"),
            };
        }

        Self {
            uri,
            exchange: "vigilo.events".to_string(),
            event_queue: "vigilo.events.domain".to_string(),
            worker_queue: "vigilo.worker".to_string(),
            worker_retry_exchange: "vigilo.worker.retry".to_string(),
            worker_quarantine_exchange: "vigilo.worker.quarantine".to_string(),
            worker_quarantine_queue: "vigilo.worker.quarantine".to_string(),
        }
    }
}

/// Raw RabbitMQ worker delivery with payload bytes and delivery metadata.
pub(crate) struct RawConsumedMessage {
    pub(crate) delivery_tag: u64,
    pub(crate) acker: Acker,
    pub(crate) body: Vec<u8>,
    pub(crate) properties: BasicProperties,
    pub(crate) exchange: String,
    pub(crate) routing_key: String,
    pub(crate) redelivered: bool,
}

/// A JSON message fetched from the worker queue.
///
/// The `raw` delivery must be passed back to `ack`, retry, or quarantine after
/// the caller has processed the payload.
pub(crate) struct ConsumedMessage {
    pub(crate) raw: RawConsumedMessage,
    pub(crate) payload: Value,
}

impl ConsumedMessage {
    pub(crate) fn delivery_tag(&self) -> u64 {
        self.raw.delivery_tag
    }
}

/// Lazily initialized RabbitMQ broker session.
///
/// The client owns connection setup and broker declarations so command code can
/// work in terms of application events instead of `lapin` primitives. The
/// cached session is invalidated after connection/channel loss and rebuilt by
/// the next broker operation.
pub(crate) struct Client {
    config: Config,
    session: Mutex<Option<Arc<BrokerSession>>>,
}

struct BrokerSession {
    connection: Connection,
    publish_channel: Channel,
}

impl Client {
    /// Creates a client shell; no network connection is opened until first use.
    pub(crate) fn new(config: Config) -> Self {
        Self {
            config,
            session: Mutex::new(None),
        }
    }

    fn is_reconnectable_lapin_error(err: &LapinError) -> bool {
        matches!(
            err,
            LapinError::InvalidChannel(_)
                | LapinError::InvalidChannelState(_)
                | LapinError::InvalidConnectionState(_)
                | LapinError::IOError(_)
                | LapinError::MissingHeartbeatError
        )
    }

    fn is_reconnectable_anyhow_error(err: &anyhow::Error) -> bool {
        let message = err.to_string().to_ascii_lowercase();
        message.contains("invalid channel")
            || message.contains("invalid connection")
            || message.contains("io error")
            || message.contains("heartbeat")
            || message.contains("connection closed")
            || message.contains("channel closed")
    }

    /// Drops the cached broker session so the next operation reconnects and
    /// re-declares topology.
    pub(crate) async fn invalidate_session(&self) {
        let mut session = self.session.lock().await;
        if session.take().is_some() {
            warn!("invalidated rabbitmq session; next operation will reconnect");
        }
    }

    async fn get_or_connect_session(&self) -> anyhow::Result<Arc<BrokerSession>> {
        let mut session = self.session.lock().await;
        if let Some(existing) = session.as_ref() {
            return Ok(existing.clone());
        }

        let connected = Arc::new(self.connect_session().await?);
        *session = Some(connected.clone());
        Ok(connected)
    }

    async fn connect_session(&self) -> anyhow::Result<BrokerSession> {
        debug!("initializing rabbitmq connection");
        let connection = Connection::connect(&self.config.uri, ConnectionProperties::default())
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq connection failed: {}", err))?;
        let publish_channel = connection
            .create_channel()
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq channel creation failed: {}", err))?;

        self.declare_topology(&publish_channel).await?;
        publish_channel
            .confirm_select(ConfirmSelectOptions::default())
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq confirm-select failed: {}", err))?;

        Ok(BrokerSession {
            connection,
            publish_channel,
        })
    }

    fn retry_queue_name(&self, retry_bucket_name: &str) -> String {
        retry_bucket_name.replacen("vigilo.worker.retry", &self.config.worker_retry_exchange, 1)
    }

    /// Ensures durable queues and bindings exist on the provided channel.
    async fn declare_topology(&self, channel: &Channel) -> anyhow::Result<()> {
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

        channel
            .queue_declare(
                &self.config.event_queue,
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
            .map_err(|err| anyhow::anyhow!("rabbitmq event queue declaration failed: {}", err))?;

        channel
            .queue_bind(
                &self.config.event_queue,
                &self.config.exchange,
                "run.*",
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq event queue binding failed: {}", err))?;

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
                WORKER_ROUTING_KEY,
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq queue binding failed: {}", err))?;

        channel
            .exchange_declare(
                &self.config.worker_retry_exchange,
                ExchangeKind::Direct,
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
                anyhow::anyhow!("rabbitmq worker retry exchange declaration failed: {}", err)
            })?;

        channel
            .queue_bind(
                &self.config.worker_queue,
                &self.config.worker_retry_exchange,
                WORKER_ROUTING_KEY,
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await
            .map_err(|err| {
                anyhow::anyhow!("rabbitmq worker retry return binding failed: {}", err)
            })?;

        for (queue_name, ttl_ms) in WORKER_RETRY_BUCKETS {
            let queue_name = self.retry_queue_name(queue_name);
            let mut retry_args = FieldTable::default();
            retry_args.insert("x-message-ttl".into(), AMQPValue::LongInt(ttl_ms));
            retry_args.insert(
                "x-dead-letter-exchange".into(),
                AMQPValue::LongString(LongString::from(self.config.worker_retry_exchange.clone())),
            );
            retry_args.insert(
                "x-dead-letter-routing-key".into(),
                AMQPValue::LongString(LongString::from(WORKER_ROUTING_KEY)),
            );

            channel
                .queue_declare(
                    &queue_name,
                    QueueDeclareOptions {
                        passive: false,
                        durable: true,
                        exclusive: false,
                        auto_delete: false,
                        nowait: false,
                    },
                    retry_args,
                )
                .await
                .map_err(|err| {
                    anyhow::anyhow!(
                        "rabbitmq worker retry queue declaration failed for '{}': {}",
                        queue_name,
                        err
                    )
                })?;

            channel
                .queue_bind(
                    &queue_name,
                    &self.config.worker_retry_exchange,
                    &queue_name,
                    QueueBindOptions::default(),
                    FieldTable::default(),
                )
                .await
                .map_err(|err| {
                    anyhow::anyhow!(
                        "rabbitmq worker retry queue binding failed for '{}': {}",
                        queue_name,
                        err
                    )
                })?;
        }

        channel
            .exchange_declare(
                &self.config.worker_quarantine_exchange,
                ExchangeKind::Direct,
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
                anyhow::anyhow!(
                    "rabbitmq worker quarantine exchange declaration failed: {}",
                    err
                )
            })?;

        channel
            .queue_declare(
                &self.config.worker_quarantine_queue,
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
            .map_err(|err| {
                anyhow::anyhow!(
                    "rabbitmq worker quarantine queue declaration failed: {}",
                    err
                )
            })?;

        channel
            .queue_bind(
                &self.config.worker_quarantine_queue,
                &self.config.worker_quarantine_exchange,
                &self.config.worker_quarantine_queue,
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await
            .map_err(|err| {
                anyhow::anyhow!("rabbitmq worker quarantine queue binding failed: {}", err)
            })?;

        Ok(())
    }

    async fn publish_bytes_with_properties(
        &self,
        exchange: &str,
        routing_key: &str,
        body: &[u8],
        properties: BasicProperties,
    ) -> anyhow::Result<()> {
        let publish_result = self
            .publish_bytes_with_properties_once(exchange, routing_key, body, properties.clone())
            .await;
        if let Err(err) = &publish_result
            && Self::is_reconnectable_anyhow_error(err)
        {
            warn!(
                error = %err,
                "rabbitmq publish failed on current session; reconnecting and retrying once"
            );
            self.invalidate_session().await;
            return self
                .publish_bytes_with_properties_once(exchange, routing_key, body, properties)
                .await;
        }

        publish_result
    }

    async fn publish_bytes_with_properties_once(
        &self,
        exchange: &str,
        routing_key: &str,
        body: &[u8],
        properties: BasicProperties,
    ) -> anyhow::Result<()> {
        let session = self.get_or_connect_session().await?;
        let channel = &session.publish_channel;
        let confirmation = channel
            .basic_publish(
                exchange,
                routing_key,
                BasicPublishOptions {
                    mandatory: true,
                    ..BasicPublishOptions::default()
                },
                body,
                properties,
            )
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq publish failed: {}", err))?
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq publish confirmation failed: {}", err))?;

        match confirmation {
            Confirmation::Ack(None) => Ok(()),
            Confirmation::Ack(Some(returned)) => {
                let route_error = returned
                    .error()
                    .map(|err| err.to_string())
                    .unwrap_or_else(|| returned.reply_text.to_string());
                anyhow::bail!(
                    "rabbitmq publish was confirmed but returned unroutable for routing key '{}': {}",
                    routing_key,
                    route_error
                );
            }
            Confirmation::Nack(returned) => {
                let route_error = returned
                    .as_deref()
                    .and_then(|message| message.error())
                    .map(|err| err.to_string())
                    .unwrap_or_else(|| "broker negatively acknowledged publish".to_string());
                anyhow::bail!(
                    "rabbitmq publish was negatively acknowledged for routing key '{}': {}",
                    routing_key,
                    route_error
                );
            }
            Confirmation::NotRequested => {
                anyhow::bail!("rabbitmq publish confirmation was not requested");
            }
        }
    }

    /// Publishes a JSON payload to the configured topic exchange.
    pub(crate) async fn publish_json(
        &self,
        routing_key: &str,
        payload: &Value,
        message_id: &str,
    ) -> anyhow::Result<()> {
        let body = serde_json::to_vec(payload)
            .map_err(|err| anyhow::anyhow!("failed to serialize message payload: {}", err))?;

        self.publish_bytes_with_properties(
            &self.config.exchange,
            routing_key,
            &body,
            BasicProperties::default()
                .with_content_type("application/json".into())
                .with_delivery_mode(2)
                .with_message_id(message_id.to_string().into()),
        )
        .await
    }

    /// Fetches one available worker message without creating a long-lived consumer.
    ///
    /// Workers currently poll with `basic_get`, which keeps the command model
    /// simple for one-shot worker mode.
    pub(crate) async fn consume_worker_message(
        &self,
    ) -> anyhow::Result<Option<RawConsumedMessage>> {
        let consume_result = self.consume_worker_message_once().await;
        if let Err(err) = &consume_result
            && Self::is_reconnectable_anyhow_error(err)
        {
            warn!(
                error = %err,
                "rabbitmq basic_get failed on current session; reconnecting and retrying once"
            );
            self.invalidate_session().await;
            return self.consume_worker_message_once().await;
        }

        consume_result
    }

    async fn consume_worker_message_once(&self) -> anyhow::Result<Option<RawConsumedMessage>> {
        let session = self.get_or_connect_session().await?;
        let channel = &session.publish_channel;

        let maybe_delivery = channel
            .basic_get(&self.config.worker_queue, BasicGetOptions::default())
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq consume failed: {}", err))?;

        let Some(delivery) = maybe_delivery else {
            return Ok(None);
        };

        Ok(Some(RawConsumedMessage {
            delivery_tag: delivery.delivery_tag,
            acker: delivery.acker.clone(),
            body: delivery.data.clone(),
            properties: delivery.properties.clone(),
            exchange: delivery.exchange.to_string(),
            routing_key: delivery.routing_key.to_string(),
            redelivered: delivery.redelivered,
        }))
    }

    /// Creates a long-lived consumer stream for worker messages using `basic_consume`.
    pub(crate) async fn consume_worker_stream(
        &self,
        consumer_tag_prefix: &str,
        prefetch: u16,
    ) -> anyhow::Result<lapin::Consumer> {
        let stream_result = self
            .consume_worker_stream_once(consumer_tag_prefix, prefetch)
            .await;
        if let Err(err) = &stream_result
            && Self::is_reconnectable_anyhow_error(err)
        {
            warn!(
                error = %err,
                "rabbitmq consumer creation failed on current session; reconnecting and retrying once"
            );
            self.invalidate_session().await;
            return self
                .consume_worker_stream_once(consumer_tag_prefix, prefetch)
                .await;
        }

        stream_result
    }

    async fn consume_worker_stream_once(
        &self,
        consumer_tag_prefix: &str,
        prefetch: u16,
    ) -> anyhow::Result<lapin::Consumer> {
        let session = self.get_or_connect_session().await?;
        let channel =
            session.connection.create_channel().await.map_err(|err| {
                anyhow::anyhow!("rabbitmq consumer channel creation failed: {}", err)
            })?;
        self.declare_topology(&channel).await?;
        channel
            .basic_qos(prefetch, BasicQosOptions { global: false })
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq qos configuration failed: {}", err))?;

        let consumer_tag = format!("{}-{}", consumer_tag_prefix, Uuid::now_v7());
        channel
            .basic_consume(
                &self.config.worker_queue,
                &consumer_tag,
                BasicConsumeOptions::default(),
                FieldTable::default(),
            )
            .await
            .map_err(|err| anyhow::anyhow!("rabbitmq consumer creation failed: {}", err))
    }

    /// Acknowledges successful processing of one delivery.
    pub(crate) async fn ack(&self, message: &RawConsumedMessage) -> anyhow::Result<()> {
        match message.acker.ack(BasicAckOptions::default()).await {
            Ok(()) => Ok(()),
            Err(err) if Self::is_reconnectable_lapin_error(&err) => {
                warn!(
                    delivery_tag = message.delivery_tag,
                    error = %err,
                    "rabbitmq ack failed after connection/channel loss; message may redeliver"
                );
                self.invalidate_session().await;
                Ok(())
            }
            Err(err) => Err(anyhow::anyhow!("rabbitmq ack failed: {}", err)),
        }
    }

    fn retry_count(properties: &BasicProperties) -> i32 {
        properties
            .headers()
            .as_ref()
            .and_then(|headers| headers.inner().get(HEADER_RETRY_COUNT))
            .and_then(|value| value.as_long_int())
            .unwrap_or(0)
            .max(0)
    }

    fn retry_queue_for_attempt(&self, attempt: i32) -> String {
        let index = usize::try_from(attempt.saturating_sub(1)).unwrap_or(usize::MAX);
        let queue_name = WORKER_RETRY_BUCKETS
            .get(index)
            .or_else(|| WORKER_RETRY_BUCKETS.last())
            .map(|(queue, _)| *queue)
            .unwrap_or("vigilo.worker.retry.30m");

        self.retry_queue_name(queue_name)
    }

    fn retry_queue_for_delay_seconds(&self, delay_seconds: i64) -> String {
        let delay_ms = delay_seconds.max(1).saturating_mul(1_000);
        let queue_name = WORKER_RETRY_BUCKETS
            .iter()
            .find(|(_, ttl_ms)| i64::from(*ttl_ms) >= delay_ms)
            .or_else(|| WORKER_RETRY_BUCKETS.last())
            .map(|(queue, _)| *queue)
            .unwrap_or("vigilo.worker.retry.30m");

        self.retry_queue_name(queue_name)
    }

    pub(crate) fn can_retry_worker_message(&self, message: &RawConsumedMessage) -> bool {
        Self::retry_count(&message.properties) < WORKER_MAX_RETRIES
    }

    fn message_headers(message: &RawConsumedMessage) -> FieldTable {
        message
            .properties
            .headers()
            .as_ref()
            .cloned()
            .unwrap_or_default()
    }

    fn insert_string_header(headers: &mut FieldTable, key: &str, value: impl Into<String>) {
        headers.insert(
            ShortString::from(key),
            AMQPValue::LongString(LongString::from(value.into())),
        );
    }

    fn retry_properties(
        message: &RawConsumedMessage,
        retry_count: i32,
        reason: &str,
        error_class: &str,
    ) -> BasicProperties {
        let mut headers = Self::message_headers(message);
        let now = chrono::Utc::now().to_rfc3339();
        if !headers.contains_key(HEADER_FIRST_FAILED_AT) {
            Self::insert_string_header(&mut headers, HEADER_FIRST_FAILED_AT, now.clone());
        }
        headers.insert(
            ShortString::from(HEADER_RETRY_COUNT),
            AMQPValue::LongInt(retry_count),
        );
        Self::insert_string_header(&mut headers, HEADER_LAST_FAILED_AT, now);
        Self::insert_string_header(&mut headers, HEADER_LAST_ERROR, reason);
        Self::insert_string_header(&mut headers, HEADER_ERROR_CLASS, error_class);
        Self::insert_string_header(&mut headers, HEADER_ORIGINAL_EXCHANGE, &message.exchange);
        Self::insert_string_header(
            &mut headers,
            HEADER_ORIGINAL_ROUTING_KEY,
            &message.routing_key,
        );

        message
            .properties
            .clone()
            .with_headers(headers)
            .with_delivery_mode(2)
    }

    pub(crate) async fn retry_worker_message(
        &self,
        message: &RawConsumedMessage,
        reason: &str,
        error_class: &str,
    ) -> anyhow::Result<()> {
        let next_retry_count = Self::retry_count(&message.properties).saturating_add(1);
        if next_retry_count > WORKER_MAX_RETRIES {
            self.quarantine_worker_message(
                message,
                &format!(
                    "worker message exhausted {} retries: {}",
                    WORKER_MAX_RETRIES, reason
                ),
                error_class,
            )
            .await?;
            return Ok(());
        }

        let retry_queue = self.retry_queue_for_attempt(next_retry_count);
        let properties = Self::retry_properties(message, next_retry_count, reason, error_class);
        self.publish_bytes_with_properties(
            &self.config.worker_retry_exchange,
            &retry_queue,
            &message.body,
            properties,
        )
        .await?;
        self.ack(message).await?;

        Ok(())
    }

    pub(crate) async fn delay_worker_message(
        &self,
        message: &RawConsumedMessage,
        delay_seconds: i64,
        reason: &str,
        error_class: &str,
    ) -> anyhow::Result<()> {
        let retry_queue = self.retry_queue_for_delay_seconds(delay_seconds);
        let retry_count = Self::retry_count(&message.properties);
        let properties = Self::retry_properties(message, retry_count, reason, error_class);
        self.publish_bytes_with_properties(
            &self.config.worker_retry_exchange,
            &retry_queue,
            &message.body,
            properties,
        )
        .await?;
        self.ack(message).await?;

        Ok(())
    }

    pub(crate) async fn quarantine_worker_message(
        &self,
        message: &RawConsumedMessage,
        reason: &str,
        error_class: &str,
    ) -> anyhow::Result<()> {
        let retry_count = Self::retry_count(&message.properties);
        let mut headers = Self::message_headers(message);
        let now = chrono::Utc::now().to_rfc3339();
        Self::insert_string_header(&mut headers, HEADER_LAST_FAILED_AT, now.clone());
        Self::insert_string_header(&mut headers, HEADER_LAST_ERROR, reason);
        Self::insert_string_header(&mut headers, HEADER_ERROR_CLASS, error_class);
        Self::insert_string_header(&mut headers, HEADER_ORIGINAL_EXCHANGE, &message.exchange);
        Self::insert_string_header(
            &mut headers,
            HEADER_ORIGINAL_ROUTING_KEY,
            &message.routing_key,
        );

        let original_payload = serde_json::from_slice::<Value>(&message.body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&message.body).to_string()));
        let envelope = serde_json::json!({
            "reason": reason,
            "error_class": error_class,
            "retry_count": retry_count,
            "original_exchange": message.exchange,
            "original_routing_key": message.routing_key,
            "redelivered": message.redelivered,
            "failed_at": now,
            "original_payload": original_payload,
        });
        let body = serde_json::to_vec(&envelope)
            .map_err(|err| anyhow::anyhow!("failed to serialize quarantine payload: {}", err))?;

        self.publish_bytes_with_properties(
            &self.config.worker_quarantine_exchange,
            &self.config.worker_quarantine_queue,
            &body,
            BasicProperties::default()
                .with_content_type("application/json".into())
                .with_delivery_mode(2)
                .with_headers(headers),
        )
        .await?;
        self.ack(message).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::Config;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn mq_config_preserves_default_topology_without_namespace() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("VIGILO_MQ_NAMESPACE");
        }

        let config = Config::new("amqp://localhost".to_string());

        assert_eq!(config.exchange, "vigilo.events");
        assert_eq!(config.event_queue, "vigilo.events.domain");
        assert_eq!(config.worker_queue, "vigilo.worker");
        assert_eq!(config.worker_retry_exchange, "vigilo.worker.retry");
        assert_eq!(config.worker_quarantine_queue, "vigilo.worker.quarantine");
    }

    #[test]
    fn mq_config_scopes_topology_with_namespace() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("VIGILO_MQ_NAMESPACE", "test-123");
        }

        let config = Config::new("amqp://localhost".to_string());

        assert_eq!(config.exchange, "vigilo.test-123.events");
        assert_eq!(config.event_queue, "vigilo.test-123.events.domain");
        assert_eq!(config.worker_queue, "vigilo.test-123.worker");
        assert_eq!(config.worker_retry_exchange, "vigilo.test-123.worker.retry");
        assert_eq!(
            config.worker_quarantine_queue,
            "vigilo.test-123.worker.quarantine"
        );

        unsafe {
            std::env::remove_var("VIGILO_MQ_NAMESPACE");
        }
    }
}
