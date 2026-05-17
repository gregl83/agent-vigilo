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
                            let payload = serde_json::from_slice::<serde_json::Value>(&delivery.data)
                                .map_err(|err| anyhow::anyhow!("failed to deserialize message payload: {}", err))?;

                            let message = crate::mq::ConsumedMessage {
                                delivery_tag: delivery.delivery_tag,
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
