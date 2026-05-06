use std::time::Duration;

use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use serde::Deserialize;
use tracing::{
    info,
    warn,
};
use uuid::Uuid;

use super::Executable;
use crate::{
    context::Context,
    db::workflows::chunk_processing,
    runtime::ServiceRunner,
};

const WORKER_TICK_SECONDS: u64 = 5;
const CHUNK_LEASE_SECONDS: i32 = 60;

#[derive(Debug, Deserialize)]
struct ChunkReadyMessage {
    run_id: Uuid,
    chunk_id: Uuid,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SubCommand {
    /// Start a worker process
    Start,

    /// Process a single worker cycle and exit
    Once,
}

#[derive(Debug, Args)]
pub(crate) struct Command {
    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

#[async_trait]
impl Executable for Command {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self.command {
            Some(SubCommand::Start) => {
                info!("starting worker process");
                handle_start(context).await
            }
            Some(SubCommand::Once) => {
                info!("running single worker cycle");
                handle_once(context).await
            }
            None => anyhow::bail!("missing worker subcommand; use `vigilo worker start`"),
        }
    }
}

async fn handle_start(context: Context) -> anyhow::Result<()> {
    ServiceRunner::new("worker")
        .tick_interval(Duration::from_secs(WORKER_TICK_SECONDS))
        .run_loop(move || {
            let context = context.clone();
            async move { run_worker_cycle(context).await }
        })
        .await
}

async fn handle_once(context: Context) -> anyhow::Result<()> {
    run_worker_cycle(context).await
}

async fn run_worker_cycle(context: Context) -> anyhow::Result<()> {
    let db = context.db().await?;
    let mq = context.mq().await?;

    let Some(message) = mq.consume_worker_message().await? else {
        info!("no worker messages available");
        return Ok(());
    };

    let payload = match serde_json::from_value::<ChunkReadyMessage>(message.payload.clone()) {
        Ok(payload) => payload,
        Err(err) => {
            mq.ack(message.delivery_tag).await?;
            warn!(error = %err, "dropping invalid chunk-ready message payload");
            return Ok(());
        }
    };

    let Some(chunk) =
        chunk_processing::claim_chunk_for_processing(db, payload.chunk_id, CHUNK_LEASE_SECONDS)
            .await?
    else {
        mq.ack(message.delivery_tag).await?;
        info!(
            chunk_id = %payload.chunk_id,
            run_id = %payload.run_id,
            "chunk not claimable; acknowledging message"
        );
        return Ok(());
    };

    let batch_result = chunk_processing::load_chunk_case_batch(db, &chunk).await;
    match batch_result {
        Ok(cases) => {
            chunk_processing::mark_chunk_completed(db, chunk.id).await?;
            mq.ack(message.delivery_tag).await?;
            info!(
                run_id = %chunk.run_id,
                chunk_id = %chunk.id,
                case_count = cases.len(),
                "loaded case batch from queue chunk"
            );
            Ok(())
        }
        Err(err) => {
            chunk_processing::release_chunk_as_pending(db, chunk.id).await?;
            mq.nack_requeue(message.delivery_tag).await?;
            warn!(
                run_id = %chunk.run_id,
                chunk_id = %chunk.id,
                error = %err,
                "failed to load chunk case batch; message requeued"
            );
            Ok(())
        }
    }
}
