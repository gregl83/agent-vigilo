use super::*;

async fn open_worker_consumer(
    mq: &crate::mq::Client,
    shutdown: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<Option<lapin::Consumer>> {
    let mut delay = Duration::from_millis(WORKER_MQ_RECONNECT_INITIAL_DELAY_MS);

    loop {
        match mq
            .consume_worker_stream("vigilo-worker", WORKER_STREAM_PREFETCH)
            .await
        {
            Ok(consumer) => return Ok(Some(consumer)),
            Err(err) => {
                mq.invalidate_session().await;
                warn!(
                    error = %err,
                    retry_after_ms = delay.as_millis() as u64,
                    "failed to open worker consumer; retrying after backoff"
                );

                tokio::select! {
                    _ = shutdown.cancelled() => return Ok(None),
                    _ = tokio::time::sleep(delay) => {}
                }

                delay = (delay * 2).min(Duration::from_millis(WORKER_MQ_RECONNECT_MAX_DELAY_MS));
            }
        }
    }
}

/// Starts the long-running worker loop.
pub(super) async fn exec(context: Context) -> anyhow::Result<()> {
    let evaluator_loader = EvaluatorLoaderService::new(context.clone());
    ServiceRunner::new("worker")
        .run(move |shutdown| {
            let context = context.clone();
            let evaluator_loader = evaluator_loader.clone();
            async move {
                // --- Open consumer stream ---
                // The stream is reopened below if RabbitMQ closes it or reports
                // a transient delivery failure.
                let mq = context.mq().await?;
                let Some(mut consumer) = open_worker_consumer(mq, &shutdown).await? else {
                    return Ok(());
                };
                let mut reconnect_delay =
                    Duration::from_millis(WORKER_MQ_RECONNECT_INITIAL_DELAY_MS);

                loop {
                    tokio::select! {
                        // --- Handle cooperative shutdown ---
                        // Shutdown wins over waiting for more deliveries.
                        _ = shutdown.cancelled() => return Ok(()),
                        delivery = consumer.next() => {
                            let Some(delivery_result) = delivery else {
                                // --- Reopen closed stream ---
                                // Recover from a closed delivery stream without
                                // exiting the worker service.
                                mq.invalidate_session().await;
                                warn!(
                                    retry_after_ms = reconnect_delay.as_millis() as u64,
                                    "worker consumer stream closed; reopening consumer after backoff"
                                );
                                tokio::select! {
                                    _ = shutdown.cancelled() => return Ok(()),
                                    _ = tokio::time::sleep(reconnect_delay) => {}
                                }
                                let Some(reopened) = open_worker_consumer(mq, &shutdown).await? else {
                                    return Ok(());
                                };
                                consumer = reopened;
                                reconnect_delay = (reconnect_delay * 2)
                                    .min(Duration::from_millis(WORKER_MQ_RECONNECT_MAX_DELAY_MS));
                                continue;
                            };

                            let delivery = match delivery_result {
                                Ok(delivery) => {
                                    reconnect_delay = Duration::from_millis(WORKER_MQ_RECONNECT_INITIAL_DELAY_MS);
                                    delivery
                                }
                                Err(err) => {
                                    mq.invalidate_session().await;
                                    warn!(
                                        error = %err,
                                        retry_after_ms = reconnect_delay.as_millis() as u64,
                                        "worker consumer delivery failed; reopening consumer after backoff"
                                    );
                                    tokio::select! {
                                        _ = shutdown.cancelled() => return Ok(()),
                                        _ = tokio::time::sleep(reconnect_delay) => {}
                                    }
                                    let Some(reopened) = open_worker_consumer(mq, &shutdown).await? else {
                                        return Ok(());
                                    };
                                    consumer = reopened;
                                    reconnect_delay = (reconnect_delay * 2)
                                        .min(Duration::from_millis(WORKER_MQ_RECONNECT_MAX_DELAY_MS));
                                    continue;
                                }
                            };

                            // --- Normalize AMQP delivery ---
                            // Convert stream deliveries into the same message
                            // type used by one-shot worker mode.
                            let raw_message = crate::mq::RawConsumedMessage {
                                delivery_tag: delivery.delivery_tag,
                                acker: delivery.acker.clone(),
                                body: delivery.data.clone(),
                                properties: delivery.properties.clone(),
                                exchange: delivery.exchange.to_string(),
                                routing_key: delivery.routing_key.to_string(),
                                redelivered: delivery.redelivered,
                            };

                            // --- Validate outer JSON body ---
                            // Reject malformed messages before handing them to
                            // the chunk workflow.
                            let payload = match serde_json::from_slice::<serde_json::Value>(&raw_message.body) {
                                Ok(payload) => payload,
                                Err(err) => {
                                    mq.quarantine_worker_message(
                                        &raw_message,
                                        &format!("worker message body was not valid JSON: {}", err),
                                        "invalid_json",
                                    )
                                    .await?;
                                    warn!(error = %err, "quarantined invalid worker message body");
                                    continue;
                                }
                            };

                            let message = crate::mq::ConsumedMessage {
                                raw: raw_message,
                                payload,
                            };

                            // --- Run shared chunk workflow ---
                            run_worker_message(context.clone(), &evaluator_loader, message).await?;
                        }
                    }
                }
            }
        })
        .await
}
