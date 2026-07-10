//! Coordinator process command.
//!
//! The coordinator drives run-level orchestration:
//! - atomically starts pending runs and dispatches chunk-ready event windows
//! - finalizes runs whose chunks/executions are terminal
//! - publishes outbox events from active database placements to messaging

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
    context::{
        Context,
        database,
    },
    db::workflows::{
        run_dispatch,
        run_finalize,
        run_shard_summary,
    },
    outbox::{
        EventPublisher,
        MqEventPublisher,
        OutboxPublishStats,
        OutboxPublisherConfig,
        publish_pending_events,
    },
    runtime::ServiceRunner,
};

mod once;
mod start;

const COORDINATOR_TICK_SECONDS: u64 = 5;
const COORDINATOR_LEASE_SECONDS: i32 = 60;
const COORDINATOR_MAX_DISPATCH_PER_CYCLE: u64 = 64;
const COORDINATOR_MAX_FINALIZE_PER_CYCLE: u64 = 64;
const RUN_CHUNK_DISPATCH_WINDOW_SIZE: i64 = 512;
const CHUNK_LEASE_RECOVERY_BATCH_SIZE: i64 = 1_000;
const CHUNK_LEASE_MAX_RECOVERIES: i32 = 3;
const OUTBOX_BATCH_SIZE: i64 = 1_000;
const OUTBOX_PUBLISH_PARALLELISM: u64 = 64;
const OUTBOX_LEASE_SECONDS: i32 = 60;
const OUTBOX_RETRY_DELAY_SECONDS: i32 = 10;

#[derive(Debug, Clone)]
struct CoordinatorRuntimeConfig {
    tick_seconds: u64,
    lease_seconds: i32,
    max_dispatch_per_cycle: usize,
    max_finalize_per_cycle: usize,
    run_chunk_dispatch_window_size: i64,
    chunk_lease_recovery_batch_size: i64,
    chunk_lease_max_recoveries: i32,
    outbox_batch_size: i64,
    outbox_publish_parallelism: usize,
    outbox_lease_seconds: i32,
    outbox_retry_delay_seconds: i32,
}

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
    /// Seconds between coordinator cycles in start mode
    #[arg(long, env = "VIGILO_COORDINATOR_TICK_SECONDS", default_value_t = COORDINATOR_TICK_SECONDS, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub tick_seconds: u64,

    /// Coordinator lease duration for run dispatch/finalization
    #[arg(long, env = "VIGILO_COORDINATOR_LEASE_SECONDS", default_value_t = COORDINATOR_LEASE_SECONDS, value_parser = clap::value_parser!(i32).range(1..=86400))]
    pub lease_seconds: i32,

    /// Maximum run-shard dispatch windows prepared per coordinator cycle
    #[arg(long, env = "VIGILO_COORDINATOR_MAX_DISPATCH_PER_CYCLE", default_value_t = COORDINATOR_MAX_DISPATCH_PER_CYCLE, value_parser = clap::value_parser!(u64).range(1..=100_000))]
    pub max_dispatch_per_cycle: u64,

    /// Maximum runs finalized per coordinator cycle
    #[arg(long, env = "VIGILO_COORDINATOR_MAX_FINALIZE_PER_CYCLE", default_value_t = COORDINATOR_MAX_FINALIZE_PER_CYCLE, value_parser = clap::value_parser!(u64).range(1..=100_000))]
    pub max_finalize_per_cycle: u64,

    /// Number of run chunks made ready per run-shard dispatch window
    #[arg(long, env = "VIGILO_RUN_CHUNK_DISPATCH_WINDOW_SIZE", default_value_t = RUN_CHUNK_DISPATCH_WINDOW_SIZE, value_parser = clap::value_parser!(i64).range(1..=1_000_000))]
    pub run_chunk_dispatch_window_size: i64,

    /// Maximum expired chunk leases recovered per coordinator cycle
    #[arg(long, env = "VIGILO_CHUNK_LEASE_RECOVERY_BATCH_SIZE", default_value_t = CHUNK_LEASE_RECOVERY_BATCH_SIZE, value_parser = clap::value_parser!(i64).range(1..=1_000_000))]
    pub chunk_lease_recovery_batch_size: i64,

    /// Maximum times an expired chunk lease can be recovered before the chunk fails
    #[arg(long, env = "VIGILO_CHUNK_LEASE_MAX_RECOVERIES", default_value_t = CHUNK_LEASE_MAX_RECOVERIES, value_parser = clap::value_parser!(i32).range(0..=10_000))]
    pub chunk_lease_max_recoveries: i32,

    /// Maximum outbox events claimed per database placement per publish pass
    #[arg(long, env = "VIGILO_OUTBOX_BATCH_SIZE", default_value_t = OUTBOX_BATCH_SIZE, value_parser = clap::value_parser!(i64).range(1..=1_000_000))]
    pub outbox_batch_size: i64,

    /// Maximum concurrent outbox broker publishes per publish pass
    #[arg(long, env = "VIGILO_OUTBOX_PUBLISH_PARALLELISM", default_value_t = OUTBOX_PUBLISH_PARALLELISM, value_parser = clap::value_parser!(u64).range(1..=10_000))]
    pub outbox_publish_parallelism: u64,

    /// Outbox publish lease duration
    #[arg(long, env = "VIGILO_OUTBOX_LEASE_SECONDS", default_value_t = OUTBOX_LEASE_SECONDS, value_parser = clap::value_parser!(i32).range(1..=86400))]
    pub outbox_lease_seconds: i32,

    /// Delay before a failed outbox publish can be retried
    #[arg(long, env = "VIGILO_OUTBOX_RETRY_DELAY_SECONDS", default_value_t = OUTBOX_RETRY_DELAY_SECONDS, value_parser = clap::value_parser!(i32).range(1..=86400))]
    pub outbox_retry_delay_seconds: i32,

    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

impl Command {
    fn runtime_config(&self) -> CoordinatorRuntimeConfig {
        CoordinatorRuntimeConfig {
            tick_seconds: self.tick_seconds,
            lease_seconds: self.lease_seconds,
            max_dispatch_per_cycle: self.max_dispatch_per_cycle as usize,
            max_finalize_per_cycle: self.max_finalize_per_cycle as usize,
            run_chunk_dispatch_window_size: self.run_chunk_dispatch_window_size,
            chunk_lease_recovery_batch_size: self.chunk_lease_recovery_batch_size,
            chunk_lease_max_recoveries: self.chunk_lease_max_recoveries,
            outbox_batch_size: self.outbox_batch_size,
            outbox_publish_parallelism: self.outbox_publish_parallelism as usize,
            outbox_lease_seconds: self.outbox_lease_seconds,
            outbox_retry_delay_seconds: self.outbox_retry_delay_seconds,
        }
    }
}

#[async_trait]
impl Executable for Command {
    /// Executes the selected coordinator mode.
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        let config = self.runtime_config();
        match self.command {
            Some(SubCommand::Start) => {
                info!("starting coordinator process");
                start::exec(context, config).await
            }
            Some(SubCommand::Once) => {
                info!("running single coordinator cycle");
                once::exec(context, config).await
            }
            None => anyhow::bail!("missing coordinator subcommand; use `vigilo coordinator start`"),
        }
    }
}

/// Executes one full coordinator cycle.
///
/// The cycle is intentionally ordered to keep run progression deterministic:
/// 1. recover expired worker chunk leases
/// 2. atomically start pending runs and dispatch chunk-ready windows
/// 3. claim/finalize finalizable runs (bounded batch)
/// 4. publish bounded batches of pending outbox events from active placements
async fn run_coordinator_cycle(
    context: Context,
    coordinator_id: Uuid,
    config: &CoordinatorRuntimeConfig,
) -> anyhow::Result<()> {
    // --- Acquire cycle services ---
    // The database service owns control and execution placement routing. The
    // queue handle is only needed for the final publish pass.
    debug!(coordinator_id = %coordinator_id, "starting coordinator cycle pre-flight");

    debug!(coordinator_id = %coordinator_id, "acquiring database context");
    let database = context.db().await?;
    debug!(coordinator_id = %coordinator_id, "database context ready");

    debug!(coordinator_id = %coordinator_id, "acquiring messaging context");
    let mq = context.mq().await?;
    debug!(coordinator_id = %coordinator_id, "messaging context ready");

    // --- Recover expired chunk leases ---
    // Recovery runs before new dispatch so dead workers do not block
    // finalization or leave ready work stranded.
    let recovery_stats = recover_expired_chunk_leases(database, coordinator_id, config).await?;

    // --- Dispatch runnable chunk windows ---
    // Dispatch is drained before finalization so newly-created work gets
    // surfaced promptly.
    let dispatch_count = drain_dispatch_batch(database, coordinator_id, config).await?;

    // --- Finalize terminal runs ---
    let finalized_count = drain_finalize_batch(database, coordinator_id, config).await?;

    // --- Publish durable outbox events ---
    // Failed broker publishes stay in the outbox delivery queue for retry.
    debug!(coordinator_id = %coordinator_id, "starting outbox publish pass");
    let publisher = MqEventPublisher::new(mq);
    let outbox_config = OutboxPublisherConfig {
        batch_size: config.outbox_batch_size,
        publish_parallelism: config.outbox_publish_parallelism,
        lease_seconds: config.outbox_lease_seconds,
        retry_delay_seconds: config.outbox_retry_delay_seconds,
    };
    let publish_stats =
        publish_outbox_events(database, &publisher, &outbox_config, coordinator_id).await?;
    info!(
        expired_chunk_leases_recovered = recovery_stats.recovered,
        expired_chunk_leases_failed = recovery_stats.failed,
        dispatch_windows_prepared = dispatch_count,
        runs_finalized = finalized_count,
        outbox_events_claimed = publish_stats.claimed,
        outbox_events_published = publish_stats.published,
        outbox_events_failed = publish_stats.failed,
        outbox_stale_claims = publish_stats.stale_claims,
        "completed outbox publish cycle"
    );

    debug!(coordinator_id = %coordinator_id, "coordinator cycle complete");

    Ok(())
}

async fn recover_expired_chunk_leases(
    database: &database::Db,
    coordinator_id: Uuid,
    config: &CoordinatorRuntimeConfig,
) -> anyhow::Result<run_dispatch::ChunkLeaseRecoveryStats> {
    // --- Expired lease recovery pass ---
    // The workflow returns both recovered chunks and chunks failed after
    // exhausting recovery attempts.
    debug!(coordinator_id = %coordinator_id, "recovering expired chunk leases");

    let aliases = database.active_execution_database_aliases().await?;
    let mut stats = run_dispatch::ChunkLeaseRecoveryStats::default();

    for alias in aliases {
        let db = database.placement(&alias).await?;
        let alias_stats = run_dispatch::recover_expired_chunk_leases(
            db,
            config.chunk_lease_max_recoveries,
            config.chunk_lease_recovery_batch_size,
        )
        .await?;

        stats.recovered += alias_stats.recovered;
        stats.failed += alias_stats.failed;

        if alias_stats.recovered > 0 || alias_stats.failed > 0 {
            info!(
                coordinator_id = %coordinator_id,
                database_alias = %alias,
                expired_chunk_leases_recovered = alias_stats.recovered,
                expired_chunk_leases_failed = alias_stats.failed,
                "completed expired chunk lease recovery pass for execution placement"
            );
        } else {
            debug!(
                coordinator_id = %coordinator_id,
                database_alias = %alias,
                "no expired chunk leases recovered for execution placement"
            );
        }
    }

    if stats.recovered == 0 && stats.failed == 0 {
        debug!(
            coordinator_id = %coordinator_id,
            "no expired chunk leases recovered for this cycle"
        );
    } else {
        info!(
            coordinator_id = %coordinator_id,
            expired_chunk_leases_recovered = stats.recovered,
            expired_chunk_leases_failed = stats.failed,
            "completed expired chunk lease recovery pass"
        );
    }

    Ok(stats)
}

async fn drain_dispatch_batch(
    database: &database::Db,
    coordinator_id: Uuid,
    config: &CoordinatorRuntimeConfig,
) -> anyhow::Result<usize> {
    // --- Dispatch drain pass ---
    // Repeatedly claim one dispatchable run/window until the cycle limit is
    // reached or no pending dispatch work remains.
    debug!(coordinator_id = %coordinator_id, "draining dispatchable run-shard windows");

    let mut dispatched = 0usize;
    for _ in 0..config.max_dispatch_per_cycle {
        let control_db = database.control().await?;
        let Some(route) = run_dispatch::select_next_dispatch_route(control_db).await? else {
            break;
        };

        let Some(snapshot) = run_dispatch::prepare_dispatch_run_snapshot(
            control_db,
            &route,
            coordinator_id,
            config.lease_seconds,
        )
        .await?
        else {
            continue;
        };

        let execution_db = database.execution(route.run_id, route.run_shard).await?;
        let Some(run) = run_dispatch::dispatch_routed_run_window(
            execution_db,
            config.run_chunk_dispatch_window_size,
            &route,
            &snapshot,
        )
        .await?
        else {
            continue;
        };

        dispatched += 1;
        debug!(
            run_id = %run.id,
            run_key = %run.run_key,
            run_shard = run.run_shard,
            "claimed dispatchable run shard window"
        );
        info!(
            run_id = %run.id,
            run_key = %run.run_key,
            run_shard = run.run_shard,
            chunk_events_enqueued = run.chunk_events_enqueued,
            chunks_marked_dispatched = run.chunks_marked_dispatched,
            run_started_events_enqueued = run.run_started_events_enqueued,
            "prepared bounded dispatch shard window"
        );
    }

    if dispatched == 0 {
        info!("no dispatchable run-shard windows available for coordinator cycle");
    } else {
        info!(
            coordinator_id = %coordinator_id,
            dispatch_windows_prepared = dispatched,
            "completed coordinator dispatch drain pass"
        );
    }

    Ok(dispatched)
}

async fn publish_outbox_events(
    database: &database::Db,
    publisher: &dyn EventPublisher,
    config: &OutboxPublisherConfig,
    coordinator_id: Uuid,
) -> anyhow::Result<OutboxPublishStats> {
    let aliases = database.active_outbox_database_aliases().await?;
    let mut stats = OutboxPublishStats::default();

    for alias in aliases {
        let db = database.placement(&alias).await?;
        let alias_stats = publish_pending_events(db, publisher, config).await?;

        stats.claimed += alias_stats.claimed;
        stats.published += alias_stats.published;
        stats.failed += alias_stats.failed;
        stats.stale_claims += alias_stats.stale_claims;

        if alias_stats.claimed > 0 {
            info!(
                coordinator_id = %coordinator_id,
                database_alias = %alias,
                outbox_events_claimed = alias_stats.claimed,
                outbox_events_published = alias_stats.published,
                outbox_events_failed = alias_stats.failed,
                outbox_stale_claims = alias_stats.stale_claims,
                "completed outbox publish pass for database placement"
            );
        } else {
            debug!(
                coordinator_id = %coordinator_id,
                database_alias = %alias,
                "no publishable outbox events for database placement"
            );
        }
    }

    Ok(stats)
}

async fn drain_finalize_batch(
    database: &database::Db,
    coordinator_id: Uuid,
    config: &CoordinatorRuntimeConfig,
) -> anyhow::Result<usize> {
    // --- Finalization drain pass ---
    // Repeatedly claim one finalizable run until the cycle limit is reached or
    // no run has all chunks terminal.
    debug!(coordinator_id = %coordinator_id, "draining finalizable runs");

    let mut finalized = 0usize;
    for _ in 0..config.max_finalize_per_cycle {
        let control_db = database.control().await?;
        let Some(run) = run_finalize::select_next_finalization_candidate(control_db).await? else {
            break;
        };

        let summaries = collect_run_shard_summaries(database, run.id).await?;
        if summaries.is_empty() || summaries.iter().any(|summary| !summary.is_terminal()) {
            debug!(
                run_id = %run.id,
                run_key = %run.run_key,
                shard_summary_count = summaries.len(),
                "finalization candidate is waiting for terminal shard summaries"
            );
            break;
        }

        let Some(claimed) = run_finalize::claim_finalization_candidate(
            control_db,
            run.id,
            coordinator_id,
            config.lease_seconds,
        )
        .await?
        else {
            continue;
        };

        debug!(run_id = %claimed.id, run_key = %claimed.run_key, "claimed run for finalization");
        if let Some(done) =
            run_finalize::finalize_claimed_run_from_summaries(control_db, claimed.id, &summaries)
                .await?
        {
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
            debug!(run_id = %claimed.id, "claimed finalizable run but no finalization update was applied");
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

async fn collect_run_shard_summaries(
    database: &database::Db,
    run_id: Uuid,
) -> anyhow::Result<Vec<run_shard_summary::RunShardSummary>> {
    let routes = database.execution_routes_for_run(run_id).await?;
    let mut summaries = Vec::with_capacity(routes.len());

    for (run_shard, alias, db) in routes {
        let Some(summary) =
            run_shard_summary::select_run_shard_summary(&db, run_id, run_shard).await?
        else {
            debug!(
                run_id = %run_id,
                run_shard,
                database_alias = %alias,
                "run shard summary is not available yet"
            );
            return Ok(Vec::new());
        };

        debug!(
            run_id = %summary.run_id,
            run_shard = summary.run_shard,
            database_alias = %alias,
            status = %summary.status,
            terminal_execution_count = summary.terminal_execution_count,
            expected_execution_count = summary.expected_execution_count,
            "loaded run shard summary for finalization"
        );
        summaries.push(summary);
    }

    Ok(summaries)
}
