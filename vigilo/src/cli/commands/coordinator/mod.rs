//! Coordinator process command.
//!
//! The coordinator drives run-level orchestration:
//! - atomically starts pending runs and dispatches chunk-ready event windows
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
    outbox::{
        MqEventPublisher,
        OutboxPublisherConfig,
        publish_pending_events,
    },
    runtime::ServiceRunner,
};

mod once;
mod start;

const COORDINATOR_TICK_SECONDS: u64 = 5;
const COORDINATOR_LEASE_SECONDS: i32 = 60;
const COORDINATOR_MAX_DISPATCH_PER_CYCLE: usize = 64;
const COORDINATOR_MAX_FINALIZE_PER_CYCLE: usize = 64;
const RUN_CHUNK_DISPATCH_WINDOW_SIZE: i64 = 512;
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
                start::exec(context).await
            }
            Some(SubCommand::Once) => {
                info!("running single coordinator cycle");
                once::exec(context).await
            }
            None => anyhow::bail!("missing coordinator subcommand; use `vigilo coordinator start`"),
        }
    }
}

/// Executes one full coordinator cycle.
///
/// The cycle is intentionally ordered to keep run progression deterministic:
/// 1. atomically start pending runs and dispatch chunk-ready windows
/// 2. claim/finalize finalizable runs (bounded batch)
/// 3. publish a bounded batch of pending outbox events
async fn run_coordinator_cycle(context: Context, coordinator_id: Uuid) -> anyhow::Result<()> {
    debug!(coordinator_id = %coordinator_id, "starting coordinator cycle pre-flight");

    debug!(coordinator_id = %coordinator_id, "acquiring database context");
    let db = context.db().await?;
    debug!(coordinator_id = %coordinator_id, "database context ready");

    debug!(coordinator_id = %coordinator_id, "acquiring messaging context");
    let mq = context.mq().await?;
    debug!(coordinator_id = %coordinator_id, "messaging context ready");

    let dispatch_count = drain_dispatch_batch(db, coordinator_id).await?;
    let finalized_count = drain_finalize_batch(db, coordinator_id).await?;

    debug!(coordinator_id = %coordinator_id, "starting outbox publish pass");
    let publisher = MqEventPublisher::new(mq);
    let outbox_config = OutboxPublisherConfig {
        batch_size: OUTBOX_BATCH_SIZE,
        lease_seconds: OUTBOX_LEASE_SECONDS,
        retry_delay_seconds: OUTBOX_RETRY_DELAY_SECONDS,
    };
    let publish_stats = publish_pending_events(db, &publisher, &outbox_config).await?;
    info!(
        dispatch_windows_prepared = dispatch_count,
        runs_finalized = finalized_count,
        outbox_events_claimed = publish_stats.claimed,
        outbox_events_published = publish_stats.published,
        outbox_events_failed = publish_stats.failed,
        "completed outbox publish cycle"
    );

    debug!(coordinator_id = %coordinator_id, "coordinator cycle complete");

    Ok(())
}

async fn drain_dispatch_batch(db: &sqlx::PgPool, coordinator_id: Uuid) -> anyhow::Result<usize> {
    debug!(coordinator_id = %coordinator_id, "draining dispatchable run windows");

    let mut dispatched = 0usize;
    for _ in 0..COORDINATOR_MAX_DISPATCH_PER_CYCLE {
        let Some(run) = run_dispatch::dispatch_next_run_window(
            db,
            coordinator_id,
            COORDINATOR_LEASE_SECONDS,
            RUN_CHUNK_DISPATCH_WINDOW_SIZE,
        )
        .await?
        else {
            break;
        };

        dispatched += 1;
        debug!(run_id = %run.id, run_key = %run.run_key, "claimed dispatchable run window");
        info!(
            run_id = %run.id,
            run_key = %run.run_key,
            chunk_events_enqueued = run.chunk_events_enqueued,
            chunks_marked_dispatched = run.chunks_marked_dispatched,
            run_started_events_enqueued = run.run_started_events_enqueued,
            "prepared bounded dispatch window"
        );
    }

    if dispatched == 0 {
        info!("no dispatchable run windows available for coordinator cycle");
    } else {
        info!(
            coordinator_id = %coordinator_id,
            dispatch_windows_prepared = dispatched,
            "completed coordinator dispatch drain pass"
        );
    }

    Ok(dispatched)
}

async fn drain_finalize_batch(db: &sqlx::PgPool, coordinator_id: Uuid) -> anyhow::Result<usize> {
    debug!(coordinator_id = %coordinator_id, "draining finalizable runs");

    let mut finalized = 0usize;
    for _ in 0..COORDINATOR_MAX_FINALIZE_PER_CYCLE {
        let Some(run) =
            run_finalize::claim_next_finalizable_run(db, coordinator_id, COORDINATOR_LEASE_SECONDS)
                .await?
        else {
            break;
        };

        debug!(run_id = %run.id, run_key = %run.run_key, "claimed run for finalization");
        if let Some(done) = run_finalize::finalize_claimed_run(db, run.id).await? {
            finalized += 1;
            info!(
                run_id = %done.id,
                run_key = %done.run_key,
                gate_status = %done.gate_status,
                terminal_execution_count = done.terminal_execution_count,
                passed_execution_count = done.passed_execution_count,
                failed_execution_count = done.failed_execution_count,
                errored_execution_count = done.errored_execution_count,
                "finalized run and enqueued completion event"
            );
        } else {
            debug!(run_id = %run.id, "claimed finalizable run but no finalization update was applied");
        }
    }

    if finalized == 0 {
        debug!(
            coordinator_id = %coordinator_id,
            "no finalizable runs available for this cycle"
        );
    } else {
        info!(
            coordinator_id = %coordinator_id,
            runs_finalized = finalized,
            "completed coordinator finalization drain pass"
        );
    }

    Ok(finalized)
}
