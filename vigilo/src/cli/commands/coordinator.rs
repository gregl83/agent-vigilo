//! Coordinator process command.
//!
//! The coordinator drives run-level orchestration:
//! - resumes durable multi-database run creation
//! - atomically starts fully created runs and dispatches chunk-ready windows
//! - finalizes runs whose chunks/executions are terminal
//! - publishes outbox events from active database placements to messaging

use std::time::{
    Duration,
    Instant,
};

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
    db::{
        tables::outbox_events,
        workflows::{
            run_creation,
            run_dispatch,
            run_finalize,
            run_shard_summary,
        },
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
const COORDINATOR_MAX_CREATE_RECOVERY_PER_CYCLE: u64 = 16;
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
    max_create_recovery_per_cycle: usize,
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

    /// Coordinator lease duration for creation recovery, dispatch, and finalization
    #[arg(long, env = "VIGILO_COORDINATOR_LEASE_SECONDS", default_value_t = COORDINATOR_LEASE_SECONDS, value_parser = clap::value_parser!(i32).range(1..=86400))]
    pub lease_seconds: i32,

    /// Maximum incomplete run creations recovered per coordinator cycle
    #[arg(long, env = "VIGILO_COORDINATOR_MAX_CREATE_RECOVERY_PER_CYCLE", default_value_t = COORDINATOR_MAX_CREATE_RECOVERY_PER_CYCLE, value_parser = clap::value_parser!(u64).range(1..=100_000))]
    pub max_create_recovery_per_cycle: u64,

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
            max_create_recovery_per_cycle: self.max_create_recovery_per_cycle as usize,
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
/// 1. resume stale multi-database run creation
/// 2. recover expired worker chunk leases
/// 3. atomically start pending runs and dispatch chunk-ready windows
/// 4. claim/finalize finalizable runs (bounded batch)
/// 5. publish bounded batches of pending outbox events from active placements
async fn run_coordinator_cycle(
    context: Context,
    coordinator_id: Uuid,
    config: &CoordinatorRuntimeConfig,
) -> anyhow::Result<()> {
    let cycle_started = Instant::now();
    // --- Acquire cycle services ---
    // The database service owns control and execution placement routing. The
    // queue handle is acquired only after database-only recovery and dispatch.
    debug!(coordinator_id = %coordinator_id, "starting coordinator cycle pre-flight");

    debug!(coordinator_id = %coordinator_id, "acquiring database context");
    let database = context.db().await?;
    debug!(coordinator_id = %coordinator_id, "database context ready");

    // --- Resume incomplete run creation ---
    let creation_recovery_started = Instant::now();
    let creation_recovery = run_creation::recover_creating_runs(
        database,
        coordinator_id,
        config.lease_seconds,
        config.max_create_recovery_per_cycle,
    )
    .await?;
    let creation_recovery_ms = creation_recovery_started.elapsed().as_millis() as u64;

    // --- Recover expired chunk leases ---
    // Recovery runs before new dispatch so dead workers do not block
    // finalization or leave ready work stranded.
    let recovery_started = Instant::now();
    let recovery_stats = recover_expired_chunk_leases(database, coordinator_id, config).await?;
    let recovery_ms = recovery_started.elapsed().as_millis() as u64;

    // --- Dispatch runnable chunk windows ---
    // Dispatch is drained before finalization so newly-created work gets
    // surfaced promptly.
    let dispatch_started = Instant::now();
    let dispatch_count = drain_dispatch_batch(database, coordinator_id, config).await?;
    let dispatch_ms = dispatch_started.elapsed().as_millis() as u64;

    // --- Finalize terminal runs ---
    let finalization_started = Instant::now();
    let finalized_count = drain_finalize_batch(database, coordinator_id, config).await?;
    let finalization_ms = finalization_started.elapsed().as_millis() as u64;

    // --- Publish durable outbox events ---
    // Failed broker publishes stay in the outbox delivery queue for retry.
    debug!(coordinator_id = %coordinator_id, "acquiring messaging context");
    let mq = context.mq().await?;
    debug!(coordinator_id = %coordinator_id, "messaging context ready");
    debug!(coordinator_id = %coordinator_id, "starting outbox publish pass");
    let publisher = MqEventPublisher::new(mq);
    let outbox_config = OutboxPublisherConfig {
        batch_size: config.outbox_batch_size,
        publish_parallelism: config.outbox_publish_parallelism,
        lease_seconds: config.outbox_lease_seconds,
        retry_delay_seconds: config.outbox_retry_delay_seconds,
    };
    let outbox_started = Instant::now();
    let publish_stats =
        publish_outbox_events(database, &publisher, &outbox_config, coordinator_id).await?;
    let outbox_publish_ms = outbox_started.elapsed().as_millis() as u64;
    info!(
        run_creations_claimed = creation_recovery.claimed,
        run_creations_completed = creation_recovery.completed,
        run_creations_deferred = creation_recovery.deferred,
        run_creations_failed = creation_recovery.failed,
        creation_recovery_ms,
        expired_chunk_leases_recovered = recovery_stats.recovered,
        expired_chunk_leases_failed = recovery_stats.failed,
        recovery_ms,
        dispatch_windows_prepared = dispatch_count,
        dispatch_ms,
        runs_finalized = finalized_count,
        finalization_ms,
        outbox_events_claimed = publish_stats.claimed,
        outbox_events_published = publish_stats.published,
        outbox_events_failed = publish_stats.failed,
        outbox_stale_claims = publish_stats.stale_claims,
        outbox_publish_ms,
        coordinator_cycle_ms = cycle_started.elapsed().as_millis() as u64,
        "completed coordinator cycle"
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

    let alias_list_started = Instant::now();
    let aliases = database.active_execution_database_aliases().await?;
    debug!(
        coordinator_id = %coordinator_id,
        active_execution_placement_count = aliases.len(),
        active_execution_alias_list_ms = alias_list_started.elapsed().as_millis() as u64,
        "listed active execution placements for recovery"
    );
    let mut stats = run_dispatch::ChunkLeaseRecoveryStats::default();

    for alias in aliases {
        let db = database.placement(&alias).await?;
        let recovery_started = Instant::now();
        let alias_stats = run_dispatch::recover_expired_chunk_leases(
            db,
            config.chunk_lease_max_recoveries,
            config.chunk_lease_recovery_batch_size,
        )
        .await?;
        let recovery_ms = recovery_started.elapsed().as_millis() as u64;

        stats.recovered += alias_stats.recovered;
        stats.failed += alias_stats.failed;

        if alias_stats.recovered > 0 || alias_stats.failed > 0 {
            info!(
                coordinator_id = %coordinator_id,
                database_alias = %alias,
                expired_chunk_leases_recovered = alias_stats.recovered,
                expired_chunk_leases_failed = alias_stats.failed,
                recovery_ms,
                "completed expired chunk lease recovery pass for execution placement"
            );
        } else {
            debug!(
                coordinator_id = %coordinator_id,
                database_alias = %alias,
                recovery_ms,
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
    let mut dispatched_by_alias = std::collections::BTreeMap::<String, usize>::new();
    let control_db = database.control().await?;
    let dispatch_backlog = run_dispatch::count_dispatch_cursor_backlog(control_db).await?;
    info!(
        coordinator_id = %coordinator_id,
        dispatch_cursor_backlog = dispatch_backlog,
        dispatch_cycle_limit = config.max_dispatch_per_cycle,
        "measured dispatch cursor backlog"
    );
    for _ in 0..config.max_dispatch_per_cycle {
        let select_started = Instant::now();
        let Some(route) = run_dispatch::select_next_dispatch_route(control_db).await? else {
            break;
        };
        let dispatch_route_select_ms = select_started.elapsed().as_millis() as u64;
        let database_alias = route.database_alias.clone();

        debug!(
            coordinator_id = %coordinator_id,
            run_id = %route.run_id,
            run_shard = route.run_shard,
            database_alias = %database_alias,
            placement_status = %route.placement_status,
            dispatch_route_select_ms,
            routing_decision = "selected_dispatch_route",
            "selected dispatch route"
        );

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

        let execution_pool_started = Instant::now();
        let execution_db = database.execution(route.run_id, route.run_shard).await?;
        let execution_pool_resolution_ms = execution_pool_started.elapsed().as_millis() as u64;
        let dispatch_started = Instant::now();
        let Some(run) = run_dispatch::dispatch_routed_run_window(
            control_db,
            execution_db,
            config.run_chunk_dispatch_window_size,
            &route,
            &snapshot,
        )
        .await?
        else {
            continue;
        };
        let dispatch_window_ms = dispatch_started.elapsed().as_millis() as u64;

        dispatched += 1;
        *dispatched_by_alias
            .entry(database_alias.clone())
            .or_default() += 1;
        debug!(
            run_id = %run.id,
            run_key = %run.run_key,
            run_shard = run.run_shard,
            database_alias = %database_alias,
            routing_decision = "dispatch_window_claimed",
            "claimed dispatchable run shard window"
        );
        info!(
            run_id = %run.id,
            run_key = %run.run_key,
            run_shard = run.run_shard,
            database_alias = %database_alias,
            chunk_events_enqueued = run.chunk_events_enqueued,
            chunks_marked_dispatched = run.chunks_marked_dispatched,
            run_started_events_enqueued = run.run_started_events_enqueued,
            execution_pool_resolution_ms,
            dispatch_window_ms,
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
        for (alias, alias_dispatched) in dispatched_by_alias {
            info!(
                coordinator_id = %coordinator_id,
                database_alias = %alias,
                dispatch_windows_prepared = alias_dispatched,
                "completed coordinator dispatch drain pass for execution placement"
            );
        }
    }

    Ok(dispatched)
}

async fn publish_outbox_events(
    database: &database::Db,
    publisher: &dyn EventPublisher,
    config: &OutboxPublisherConfig,
    coordinator_id: Uuid,
) -> anyhow::Result<OutboxPublishStats> {
    let alias_list_started = Instant::now();
    let aliases = database.active_outbox_database_aliases().await?;
    debug!(
        coordinator_id = %coordinator_id,
        active_outbox_placement_count = aliases.len(),
        active_outbox_alias_list_ms = alias_list_started.elapsed().as_millis() as u64,
        "listed active outbox placements"
    );
    let mut stats = OutboxPublishStats::default();

    for alias in aliases {
        let db = database.placement(&alias).await?;
        let backlog_started = Instant::now();
        let outbox_backlog = outbox_events::count_publishable_outbox_backlog(db).await?;
        let outbox_backlog_query_ms = backlog_started.elapsed().as_millis() as u64;
        let publish_started = Instant::now();
        let alias_stats = publish_pending_events(db, publisher, config).await?;
        let outbox_publish_ms = publish_started.elapsed().as_millis() as u64;

        stats.claimed += alias_stats.claimed;
        stats.published += alias_stats.published;
        stats.failed += alias_stats.failed;
        stats.stale_claims += alias_stats.stale_claims;

        if alias_stats.claimed > 0 {
            info!(
                coordinator_id = %coordinator_id,
                database_alias = %alias,
                outbox_backlog,
                outbox_backlog_query_ms,
                outbox_events_claimed = alias_stats.claimed,
                outbox_events_published = alias_stats.published,
                outbox_events_failed = alias_stats.failed,
                outbox_stale_claims = alias_stats.stale_claims,
                outbox_publish_ms,
                "completed outbox publish pass for database placement"
            );
        } else {
            debug!(
                coordinator_id = %coordinator_id,
                database_alias = %alias,
                outbox_backlog,
                outbox_backlog_query_ms,
                outbox_publish_ms,
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
    let control_db = database.control().await?;
    let finalization_backlog =
        run_finalize::select_finalization_candidate_backlog(control_db).await?;
    info!(
        coordinator_id = %coordinator_id,
        finalization_candidate_backlog = finalization_backlog.candidate_count,
        finalization_oldest_candidate_lag_seconds =
            finalization_backlog.oldest_candidate_lag_seconds,
        finalization_cycle_limit = config.max_finalize_per_cycle,
        "measured finalization candidate backlog"
    );
    for _ in 0..config.max_finalize_per_cycle {
        let select_started = Instant::now();
        let Some(run) = run_finalize::select_next_finalization_candidate(control_db).await? else {
            break;
        };
        let finalization_candidate_select_ms = select_started.elapsed().as_millis() as u64;

        let summaries = collect_run_shard_summaries(database, run.id).await?;
        if summaries.is_empty() || summaries.iter().any(|summary| !summary.is_terminal()) {
            debug!(
                run_id = %run.id,
                run_key = %run.run_key,
                shard_summary_count = summaries.len(),
                finalization_candidate_select_ms,
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
        let finalize_started = Instant::now();
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
                finalization_candidate_select_ms,
                finalize_control_write_ms = finalize_started.elapsed().as_millis() as u64,
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
    let mut summaries_by_alias = std::collections::BTreeMap::<String, usize>::new();

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
        *summaries_by_alias.entry(alias).or_default() += 1;
        summaries.push(summary);
    }

    for (alias, count) in summaries_by_alias {
        debug!(
            run_id = %run_id,
            database_alias = %alias,
            shard_summaries_loaded = count,
            "loaded routed shard summaries for finalization from execution placement"
        );
    }

    Ok(summaries)
}
