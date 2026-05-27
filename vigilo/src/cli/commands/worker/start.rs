use super::*;

/// Starts the long-running worker loop.
pub(super) async fn exec(context: Context) -> anyhow::Result<()> {
    let evaluator_loader = EvaluatorLoaderService::new(context.clone());
    ServiceRunner::new("worker")
        .run(move |shutdown| {
            let context = context.clone();
            let evaluator_loader = evaluator_loader.clone();
            async move {
                let mq = context.mq().await?;
                let mut consumer = mq
                    .consume_worker_stream("vigilo-worker", WORKER_STREAM_PREFETCH)
                    .await?;

                loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => return Ok(()),
                        delivery = consumer.next() => {
                            let Some(delivery_result) = delivery else {
                                warn!("worker consumer stream closed; reopening consumer");
                                consumer = mq
                                    .consume_worker_stream("vigilo-worker", WORKER_STREAM_PREFETCH)
                                    .await?;
                                continue;
                            };

                            let delivery = delivery_result
                                .map_err(|err| anyhow::anyhow!("worker consumer delivery failed: {}", err))?;
                            let raw_message = crate::mq::RawConsumedMessage {
                                delivery_tag: delivery.delivery_tag,
                                body: delivery.data.clone(),
                                properties: delivery.properties.clone(),
                                exchange: delivery.exchange.to_string(),
                                routing_key: delivery.routing_key.to_string(),
                                redelivered: delivery.redelivered,
                            };
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
                            run_worker_message(context.clone(), &evaluator_loader, message).await?;
                        }
                    }
                }
            }
        })
        .await
}
