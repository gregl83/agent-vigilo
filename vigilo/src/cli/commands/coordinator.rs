//! Coordinator process command.
//!
//! The coordinator drives run-level orchestration:
//! - atomically dispatches pending runs into chunk-ready events
//! - finalizes runs whose chunks/executions are terminal
//! - publishes outbox events to messaging

use std::time::Duration;

use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use tracing::{
    debug,
    info,
};
use uuid::Uuid;

use super::Executable;
use crate::{
    context::Context,
    db::workflows::{
        run_dispatch,
        run_finalize,
    },
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
/// Coordinator execution modes.
pub(crate) enum SubCommand {
    /// Start a coordinator process
    Start,

    /// Run one coordinator cycle and exit
    Once,
}

#[derive(Debug, Args)]
/// Arguments for `vigilo coordinator`.
///
/// The command requires a subcommand:
/// - `start` runs the loop continuously
/// - `once` executes one orchestration cycle and exits
pub(crate) struct Command {
    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

#[async_trait]
impl Executable for Command {
    /// Executes the selected coordinator mode.
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

/// Starts the long-running coordinator loop.
async fn handle_start(context: Context) -> anyhow::Result<()> {
    // One logical coordinator id is reused across loop iterations.
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

/// Runs a single coordinator cycle with a fresh coordinator id.
///
/// Useful for cron-like orchestration or local debugging.
async fn handle_once(context: Context) -> anyhow::Result<()> {
    let coordinator_id = Uuid::now_v7().to_string();
    run_coordinator_cycle(context, &coordinator_id).await
}

/// Executes one full coordinator cycle.
///
/// The cycle is intentionally ordered to keep run progression deterministic:
/// 1. atomically claim/dispatch one pending run
/// 2. claim/finalize one finalizable run
/// 3. publish a bounded batch of pending outbox events
async fn run_coordinator_cycle(context: Context, coordinator_id: &str) -> anyhow::Result<()> {
    debug!(coordinator_id, "starting coordinator cycle pre-flight");

    debug!(coordinator_id, "acquiring database context");
    let db = context.db().await?;
    debug!(coordinator_id, "database context ready");

    debug!(coordinator_id, "acquiring messaging context");
    let mq = context.mq().await?;
    debug!(coordinator_id, "messaging context ready");

    debug!(
        coordinator_id,
        "attempting to claim pending run for dispatch"
    );

    if let Some(run) =
        run_dispatch::dispatch_next_pending_run(db, coordinator_id, COORDINATOR_LEASE_SECONDS)
            .await?
    {
        debug!(run_id = %run.id, run_key = %run.run_key, "claimed pending run");

        info!(
            run_id = %run.id,
            run_key = %run.run_key,
            chunk_events_enqueued = run.chunk_events_enqueued,
            run_started_events_enqueued = run.run_started_events_enqueued,
            "claimed run and prepared dispatch events"
        );
    } else {
        info!("no pending runs available for coordinator cycle");
    }

    debug!(coordinator_id, "attempting to claim finalizable run");

    if let Some(run) =
        run_finalize::claim_next_finalizable_run(db, coordinator_id, COORDINATOR_LEASE_SECONDS)
            .await?
    {
        debug!(run_id = %run.id, run_key = %run.run_key, "claimed run for finalization");
        if let Some(finalized) = run_finalize::finalize_claimed_run(db, run.id).await? {
            info!(
                run_id = %finalized.id,
                run_key = %finalized.run_key,
                gate_status = %finalized.gate_status,
                terminal_execution_count = finalized.terminal_execution_count,
                passed_execution_count = finalized.passed_execution_count,
                failed_execution_count = finalized.failed_execution_count,
                errored_execution_count = finalized.errored_execution_count,
                "finalized run and enqueued completion event"
            );
        } else {
            debug!(run_id = %run.id, "claimed finalizable run but no finalization update was applied");
        }
    } else {
        debug!(
            coordinator_id,
            "no finalizable runs available for this cycle"
        );
    }

    debug!(coordinator_id, "starting outbox publish pass");
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

    debug!(coordinator_id, "coordinator cycle complete");

    Ok(())
}
