use std::time::Duration;

use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use tracing::info;
use uuid::Uuid;

use super::Executable;
use crate::{
    context::Context,
    db::workflows::run_dispatch,
    outbox::publisher::{
        MqEventPublisher,
        OutboxPublisherConfig,
        publish_pending_events,
    },
    runtime::ServiceRunner,
};

const COORDINATOR_TICK_SECONDS: u64 = 5;
const COORDINATOR_LEASE_SECONDS: i32 = 60;
const OUTBOX_BATCH_SIZE: i64 = 32;
const OUTBOX_LEASE_SECONDS: i32 = 30;
const OUTBOX_RETRY_DELAY_SECONDS: i32 = 10;

#[derive(Debug, Subcommand)]
pub(crate) enum SubCommand {
    /// Start a coordinator process
    Start,

    /// Run one coordinator cycle and exit
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
                info!("starting coordinator process");
                handle_start(context).await
            }
            Some(SubCommand::Once) => {
                info!("running single coordinator cycle");
                handle_once(context).await
            }
            None => anyhow::bail!("missing coordinator subcommand; use `vigilo coordinator start`"),
        }
    }
}

async fn handle_start(context: Context) -> anyhow::Result<()> {
    let coordinator_id = Uuid::now_v7().to_string();
    ServiceRunner::new("coordinator")
        .tick_interval(Duration::from_secs(COORDINATOR_TICK_SECONDS))
        .run_loop(move || {
            let context = context.clone();
            let coordinator_id = coordinator_id.clone();
            async move { run_coordinator_cycle(context, &coordinator_id).await }
        })
        .await
}

async fn handle_once(context: Context) -> anyhow::Result<()> {
    let coordinator_id = Uuid::now_v7().to_string();
    run_coordinator_cycle(context, &coordinator_id).await
}

async fn run_coordinator_cycle(context: Context, coordinator_id: &str) -> anyhow::Result<()> {
    let db = context.db().await?;
    let mq = context.mq().await?;

    if let Some(run) =
        run_dispatch::claim_next_pending_run(db, coordinator_id, COORDINATOR_LEASE_SECONDS).await?
    {
        let chunk_events = run_dispatch::enqueue_missing_chunk_ready_events(db, run.id).await?;
        let started_events = run_dispatch::enqueue_run_started_event(db, run.id).await?;

        info!(
            run_id = %run.id,
            run_key = %run.run_key,
            chunk_events_enqueued = chunk_events,
            run_started_events_enqueued = started_events,
            "claimed run and prepared dispatch events"
        );
    } else {
        info!("no pending runs available for coordinator cycle");
    }

    let publisher = MqEventPublisher::new(mq);
    let outbox_config = OutboxPublisherConfig {
        batch_size: OUTBOX_BATCH_SIZE,
        lease_seconds: OUTBOX_LEASE_SECONDS,
        retry_delay_seconds: OUTBOX_RETRY_DELAY_SECONDS,
    };
    let publish_stats = publish_pending_events(db, &publisher, &outbox_config).await?;
    info!(
        outbox_events_claimed = publish_stats.claimed,
        outbox_events_published = publish_stats.published,
        outbox_events_failed = publish_stats.failed,
        "completed outbox publish cycle"
    );

    Ok(())
}
