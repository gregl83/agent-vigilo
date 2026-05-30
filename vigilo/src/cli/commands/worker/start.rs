use super::*;

async fn open_worker_consumer(
    mq: &crate::mq::Client,
    shutdown: &tokio_util::sync::CancellationToken,
    prefetch: u16,
) -> anyhow::Result<Option<lapin::Consumer>> {
    let mut delay = Duration::from_millis(WORKER_MQ_RECONNECT_INITIAL_DELAY_MS);

    loop {
        match mq.consume_worker_stream("vigilo-worker", prefetch).await {
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

async fn drain_in_flight_chunks(
    in_flight: &mut JoinSet<anyhow::Result<WorkerCycleOutcome>>,
) -> anyhow::Result<()> {
    while let Some(result) = in_flight.join_next().await {
        result.map_err(|err| anyhow::anyhow!("worker chunk task join failed: {}", err))??;
    }

    Ok(())
}

/// Starts the long-running worker loop.
pub(super) async fn exec(context: Context, max_inflight_chunks: u16) -> anyhow::Result<()> {
    let evaluator_loader = EvaluatorLoaderService::new(context.clone());
    let prefetch = worker_stream_prefetch(max_inflight_chunks);
    let max_inflight_chunks = usize::from(max_inflight_chunks.max(1));
    ServiceRunner::new("worker")
        .run(move |shutdown| {
            let context = context.clone();
            let evaluator_loader = evaluator_loader.clone();
            async move {
                // --- Open consumer stream ---
                // The stream is reopened below if RabbitMQ closes it or reports
                // a transient delivery failure.
                let mq = context.mq().await?;
                let Some(mut consumer) = open_worker_consumer(mq, &shutdown, prefetch).await? else {
                    return Ok(());
                };
                let mut reconnect_delay =
                    Duration::from_millis(WORKER_MQ_RECONNECT_INITIAL_DELAY_MS);
                let mut in_flight = JoinSet::new();

                loop {
                    tokio::select! {
                        // --- Handle cooperative shutdown ---
                        // Stop accepting new deliveries and let claimed chunks
                        // settle through normal ack/retry paths.
                        _ = shutdown.cancelled() => {
                            drain_in_flight_chunks(&mut in_flight).await?;
                            return Ok(());
                        },
                        task_result = in_flight.join_next(), if !in_flight.is_empty() => {
                            task_result
                                .ok_or_else(|| anyhow::anyhow!("worker chunk task set ended unexpectedly"))?
                                .map_err(|err| anyhow::anyhow!("worker chunk task join failed: {}", err))??;
                        },
                        delivery = consumer.next(), if in_flight.len() < max_inflight_chunks => {
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
                                    _ = shutdown.cancelled() => {
                                        drain_in_flight_chunks(&mut in_flight).await?;
                                        return Ok(());
                                    },
                                    _ = tokio::time::sleep(reconnect_delay) => {}
                                }
                                let Some(reopened) = open_worker_consumer(mq, &shutdown, prefetch).await? else {
                                    drain_in_flight_chunks(&mut in_flight).await?;
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
                                        _ = shutdown.cancelled() => {
                                            drain_in_flight_chunks(&mut in_flight).await?;
                                            return Ok(());
                                        },
                                        _ = tokio::time::sleep(reconnect_delay) => {}
                                    }
                                    let Some(reopened) = open_worker_consumer(mq, &shutdown, prefetch).await? else {
                                        drain_in_flight_chunks(&mut in_flight).await?;
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
                            // Multiple chunks can run concurrently only up to
                            // the configured capacity. RabbitMQ prefetch is set
                            // to the same capacity so this worker does not
                            // reserve more messages than it can process.
                            let context = context.clone();
                            let evaluator_loader = evaluator_loader.clone();
                            in_flight.spawn(async move {
                                run_worker_message(context, &evaluator_loader, message).await
                            });
                        }
                    }
                }
            }
        })
        .await
}
