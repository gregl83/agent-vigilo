//! Worker process command.
//!
//! Workers consume chunk-ready queue messages, claim work, warm evaluator
//! components, execute case-level evaluation workflows, and acknowledge or
//! requeue queue messages based on outcome. Chunk-local worker persistence uses
//! the execution database selected by `run_id + run_shard`; run profile
//! snapshots are read from that execution placement, while evaluator registry
//! metadata remains a control-plane read.

use std::{
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};

use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use futures_util::StreamExt;
use moka::future::Cache;
use serde::Deserialize;
use tokio::{
    sync::Mutex,
    task::{
        JoinHandle,
        JoinSet,
    },
};
use tokio_util::sync::CancellationToken;
use tracing::{
    debug,
    info,
    warn,
};
use uuid::Uuid;

use super::Executable;
use crate::{
    context::{
        Context,
        database::ExecutionRouteError,
    },
    contracts::run::RunProfile,
    db::{
        tables::run_snapshots,
        workflows::{
            chunk_processing,
            execution_processing,
        },
    },
    runtime::ServiceRunner,
};

mod once;
mod start;

const CHUNK_LEASE_SECONDS: i32 = 60;
const CHUNK_PROCESSING_LEASE_SECONDS: i32 = 120;
const ATTEMPT_LEASE_SECONDS: i32 = 120;
const WORKER_HEARTBEAT_INTERVAL_SECONDS: u64 = 15;
const RUN_CONTEXT_CACHE_MAX_ENTRIES: u64 = 1024;
const RUN_CONTEXT_CACHE_TTI_SECONDS: u64 = 900;
const WARMUP_PARALLELISM: usize = 8;
const DEFAULT_WORKER_MAX_INFLIGHT_CHUNKS: u16 = 1;
const WORKER_MQ_RECONNECT_INITIAL_DELAY_MS: u64 = 250;
const WORKER_MQ_RECONNECT_MAX_DELAY_MS: u64 = 30_000;
const WORKER_ROUTE_RETRY_DELAY_SECONDS: i64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerCycleOutcome {
    Processed,
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetrySettlement {
    Retried,
    Delayed,
    AcknowledgedStale,
    FailedExhausted,
}

#[derive(Debug)]
struct WorkerRunContext {
    profile: RunProfile,
    evaluator_refs: Vec<String>,
    evaluator_catalog: execution_processing::RunEvaluatorCatalog,
}

#[derive(Debug, Clone)]
struct WorkerRuntime {
    worker_id: Uuid,
    worker_host: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct WorkerRunContextKey {
    run_id: Uuid,
    run_shard: i16,
}

#[derive(Debug, Deserialize)]
/// Queue payload that signals a run chunk is ready for processing.
struct ChunkReadyMessage {
    run_id: Uuid,
    run_shard: i16,
    chunk_id: Uuid,
}

struct ChunkLeaseGuard {
    db: sqlx::PgPool,
    chunk: Mutex<crate::models::run_chunk::RunChunk>,
    lease_seconds: i32,
}

struct ChunkHeartbeat {
    guard: Arc<ChunkLeaseGuard>,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

#[derive(Debug, Default, Clone, Copy)]
/// Summary of evaluator cache warmup performed before processing a chunk.
struct WarmupStats {
    requested: usize,
    cache_hits: usize,
    loaded: usize,
}

impl WorkerRuntime {
    fn new() -> Self {
        Self {
            worker_id: Uuid::now_v7(),
            worker_host: worker_host_label(),
        }
    }
}

impl ChunkLeaseGuard {
    fn new(
        db: &sqlx::PgPool,
        chunk: crate::models::run_chunk::RunChunk,
        lease_seconds: i32,
    ) -> Arc<Self> {
        Arc::new(Self {
            db: db.clone(),
            chunk: Mutex::new(chunk),
            lease_seconds,
        })
    }

    async fn current_chunk(&self) -> crate::models::run_chunk::RunChunk {
        self.chunk.lock().await.clone()
    }

    async fn renew(&self) -> anyhow::Result<bool> {
        let current = self.current_chunk().await;
        let Some(renewed) =
            chunk_processing::extend_chunk_lease(&self.db, &current, self.lease_seconds).await?
        else {
            return Ok(false);
        };
        *self.chunk.lock().await = renewed;
        Ok(true)
    }
}

impl ChunkHeartbeat {
    fn start(
        db: &sqlx::PgPool,
        chunk: crate::models::run_chunk::RunChunk,
        runtime: WorkerRuntime,
    ) -> Self {
        let guard = ChunkLeaseGuard::new(db, chunk, CHUNK_PROCESSING_LEASE_SECONDS);
        let cancel = CancellationToken::new();
        let heartbeat_guard = guard.clone();
        let heartbeat_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_chunk_heartbeat(heartbeat_guard, runtime, heartbeat_cancel).await;
        });

        Self {
            guard,
            cancel,
            handle,
        }
    }

    async fn stop(self) -> crate::models::run_chunk::RunChunk {
        self.cancel.cancel();
        if let Err(err) = self.handle.await {
            warn!(error = %err, "worker heartbeat task failed to join");
        }
        self.guard.current_chunk().await
    }
}

fn worker_host_label() -> Option<String> {
    ["VIGILO_WORKER_HOST", "HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|key| std::env::var(key).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn run_chunk_heartbeat(
    guard: Arc<ChunkLeaseGuard>,
    runtime: WorkerRuntime,
    cancel: CancellationToken,
) {
    let mut interval =
        tokio::time::interval(Duration::from_secs(WORKER_HEARTBEAT_INTERVAL_SECONDS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = interval.tick() => {
                let chunk = guard.current_chunk().await;
                match guard.renew().await {
                    Ok(true) => {
                        match execution_processing::heartbeat_running_attempts_for_chunk(
                            &guard.db,
                            chunk.run_id,
                            chunk.run_shard,
                            chunk.id,
                            runtime.worker_id,
                            ATTEMPT_LEASE_SECONDS,
                        )
                        .await
                        {
                            Ok(renewed_attempts) => {
                                debug!(
                                    run_id = %chunk.run_id,
                                    chunk_id = %chunk.id,
                                    worker_id = %runtime.worker_id,
                                    renewed_attempts,
                                    "renewed worker chunk and attempt leases"
                                );
                            }
                            Err(err) => {
                                warn!(
                                    run_id = %chunk.run_id,
                                    chunk_id = %chunk.id,
                                    worker_id = %runtime.worker_id,
                                    error = %err,
                                    "failed to renew attempt leases during worker heartbeat"
                                );
                            }
                        }
                    }
                    Ok(false) => {
                        warn!(
                            run_id = %chunk.run_id,
                            chunk_id = %chunk.id,
                            worker_id = %runtime.worker_id,
                            "worker lost chunk lease during heartbeat"
                        );
                        return;
                    }
                    Err(err) => {
                        warn!(
                            run_id = %chunk.run_id,
                            chunk_id = %chunk.id,
                            worker_id = %runtime.worker_id,
                            error = %err,
                            "failed to renew chunk lease during worker heartbeat"
                        );
                    }
                }
            }
        }
    }
}

fn broker_message_id(message: &crate::mq::RawConsumedMessage) -> Option<String> {
    message
        .properties
        .message_id()
        .as_ref()
        .map(ToString::to_string)
}

fn worker_stream_prefetch(max_inflight_chunks: u16) -> u16 {
    max_inflight_chunks.max(1)
}

fn execution_route_is_temporarily_blocked(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<ExecutionRouteError>(),
        Some(
            ExecutionRouteError::NonDispatchableShardPlacement { .. }
                | ExecutionRouteError::StaleExecutionRoute { .. }
        )
    )
}

#[derive(Clone)]
/// Worker-local helper that resolves evaluator components into runtime cache.
///
/// This service is intentionally lightweight and command-scoped. It validates
/// evaluator identifiers and state before compiling WASM bytes into cached
/// Wasmtime components.
struct EvaluatorLoaderService {
    context: Context,
    run_context_cache: Arc<Cache<WorkerRunContextKey, Arc<WorkerRunContext>>>,
}

impl EvaluatorLoaderService {
    /// Creates a loader service bound to the current command context.
    fn new(context: Context) -> Self {
        Self {
            context,
            run_context_cache: Arc::new(
                Cache::builder()
                    .max_capacity(RUN_CONTEXT_CACHE_MAX_ENTRIES)
                    .time_to_idle(Duration::from_secs(RUN_CONTEXT_CACHE_TTI_SECONDS))
                    .build(),
            ),
        }
    }

    /// Returns a compiled component from cache or loads it from registry.
    ///
    /// Errors if:
    /// - evaluator ref format is invalid
    /// - evaluator does not exist
    /// - evaluator is in a non-runnable state
    /// - WASM compilation fails
    async fn get_or_load(
        &self,
        evaluator_ref: &str,
    ) -> anyhow::Result<wasmtime::component::Component> {
        execution_processing::get_or_load_component(&self.context, evaluator_ref).await
    }

    /// Preloads a set of evaluator refs into the registry cache.
    async fn warm_refs(&self, evaluator_refs: &[String]) -> anyhow::Result<WarmupStats> {
        let started = Instant::now();
        let mut stats = WarmupStats {
            requested: evaluator_refs.len(),
            ..WarmupStats::default()
        };

        let cache = self.context.reg().await?;
        let mut misses = Vec::new();
        for evaluator_ref in evaluator_refs {
            if cache.get(evaluator_ref).await.is_some() {
                stats.cache_hits += 1;
                continue;
            }

            misses.push(evaluator_ref.clone());
        }

        for batch in misses.chunks(WARMUP_PARALLELISM) {
            let mut tasks = JoinSet::new();
            for evaluator_ref in batch {
                let evaluator_ref = evaluator_ref.clone();
                let loader = self.clone();
                tasks.spawn(async move {
                    loader
                        .get_or_load(&evaluator_ref)
                        .await
                        .map(|_| evaluator_ref)
                });
            }

            while let Some(task_result) = tasks.join_next().await {
                let evaluator_ref = task_result
                    .map_err(|err| anyhow::anyhow!("warmup worker task join failed: {}", err))??;
                stats.loaded += 1;
                debug!(
                    evaluator_ref,
                    "loaded evaluator into runtime cache during warmup"
                );
            }
        }

        debug!(
            requested = stats.requested,
            cache_hits = stats.cache_hits,
            loaded = stats.loaded,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "completed evaluator warmup pass"
        );

        Ok(stats)
    }

    /// Returns run-scoped profile and evaluator metadata, using cache when available.
    async fn get_or_build_run_context(
        &self,
        run_id: Uuid,
        run_shard: i16,
        execution_db: &sqlx::PgPool,
    ) -> anyhow::Result<Arc<WorkerRunContext>> {
        let context = self.context.clone();
        let execution_db = execution_db.clone();
        let key = WorkerRunContextKey { run_id, run_shard };

        let context_result = self
            .run_context_cache
            .try_get_with::<_, anyhow::Error>(key, async move {
                let Some(profile_snapshot) =
                    run_snapshots::select_run_profile_snapshot(&execution_db, run_id, run_shard)
                        .await?
                else {
                    anyhow::bail!(
                        "run '{}' shard {} missing run profile snapshot",
                        run_id,
                        run_shard
                    );
                };
                let profile: RunProfile =
                    serde_json::from_value(profile_snapshot).map_err(|err| {
                        anyhow::anyhow!("run '{}' profile is invalid: {}", run_id, err)
                    })?;
                let evaluator_refs = execution_processing::evaluator_refs_from_profile(&profile)?;
                let db = context.db().await?.control().await?;
                let evaluator_catalog =
                    execution_processing::build_run_evaluator_catalog(db, &profile).await?;

                Ok(Arc::new(WorkerRunContext {
                    profile,
                    evaluator_refs,
                    evaluator_catalog,
                }))
            })
            .await;

        let context = match context_result {
            Ok(context) => context,
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "run context load failed for run '{}' (single-flight): {}",
                    run_id,
                    err
                ));
            }
        };

        Ok(context)
    }
}

#[derive(Debug, Subcommand)]
/// Worker execution modes.
pub(crate) enum SubCommand {
    /// Start a worker process
    Start {
        /// Maximum chunk messages processed concurrently by one worker process
        #[arg(long, env = "VIGILO_WORKER_MAX_INFLIGHT_CHUNKS", default_value_t = DEFAULT_WORKER_MAX_INFLIGHT_CHUNKS, value_parser = clap::value_parser!(u16).range(1..=1024))]
        max_inflight_chunks: u16,
    },

    /// Process a single worker cycle and exit
    Once,
}

#[derive(Debug, Args)]
/// Arguments for `vigilo worker`.
pub(crate) struct Command {
    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

#[async_trait]
impl Executable for Command {
    /// Executes the selected worker mode.
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self.command {
            Some(SubCommand::Start {
                max_inflight_chunks,
            }) => {
                info!("starting worker process");
                start::exec(context, WorkerRuntime::new(), max_inflight_chunks).await
            }
            Some(SubCommand::Once) => {
                info!("running single worker cycle");
                once::exec(context, WorkerRuntime::new()).await
            }
            None => anyhow::bail!("missing worker subcommand; use `vigilo worker start`"),
        }
    }
}

/// Executes a single worker cycle.
///
/// Cycle sequence:
/// 1. consume queue message
/// 2. parse payload and claim chunk lease
/// 3. warm evaluator components from run profile
/// 4. load and process the chunk case batch
/// 5. ack on success, retry with backoff on recoverable failures
async fn run_worker_drain_pass(
    context: Context,
    evaluator_loader: &EvaluatorLoaderService,
    runtime: WorkerRuntime,
    max_messages: usize,
) -> anyhow::Result<usize> {
    let mut processed = 0usize;

    for _ in 0..max_messages {
        match run_worker_cycle(context.clone(), evaluator_loader, runtime.clone()).await? {
            WorkerCycleOutcome::Processed => {
                processed += 1;
            }
            WorkerCycleOutcome::Empty => break,
        }
    }

    Ok(processed)
}

async fn run_worker_cycle(
    context: Context,
    evaluator_loader: &EvaluatorLoaderService,
    runtime: WorkerRuntime,
) -> anyhow::Result<WorkerCycleOutcome> {
    // --- Fetch worker message ---
    // `Empty` is a normal one-shot outcome, not an error.
    debug!("starting worker cycle pre-flight");

    debug!("acquiring messaging context");
    let mq = context.mq().await?;
    debug!("messaging context ready");

    debug!("attempting to consume worker message");

    let Some(raw_message) = mq.consume_worker_message().await? else {
        debug!("no worker messages available");
        return Ok(WorkerCycleOutcome::Empty);
    };

    // --- Validate outer JSON body ---
    // Invalid JSON is not retryable worker work, so it is quarantined and the
    // cycle is considered processed.
    let message = match serde_json::from_slice::<serde_json::Value>(&raw_message.body) {
        Ok(payload) => crate::mq::ConsumedMessage {
            raw: raw_message,
            payload,
        },
        Err(err) => {
            mq.quarantine_worker_message(
                &raw_message,
                &format!("worker message body was not valid JSON: {}", err),
                "invalid_json",
            )
            .await?;
            warn!("quarantined invalid worker message body");
            return Ok(WorkerCycleOutcome::Processed);
        }
    };

    run_worker_message(context, evaluator_loader, runtime, message).await
}

async fn settle_retryable_chunk_failure(
    db: &sqlx::PgPool,
    mq: &crate::mq::Client,
    message: &crate::mq::ConsumedMessage,
    chunk: &crate::models::run_chunk::RunChunk,
    reason: &str,
    error_class: &str,
) -> anyhow::Result<RetrySettlement> {
    // This settlement path is for real worker failures: warmup, DB load,
    // processing, or terminal-transition errors. These consume the bounded
    // RabbitMQ retry budget and eventually fail the chunk if the worker still
    // owns the lease.
    if mq.can_retry_worker_message(&message.raw) {
        let released = chunk_processing::release_chunk_as_pending(db, chunk).await?;
        if released > 0 {
            mq.retry_worker_message(&message.raw, reason, error_class)
                .await?;
            Ok(RetrySettlement::Retried)
        } else {
            mq.ack(&message.raw).await?;
            Ok(RetrySettlement::AcknowledgedStale)
        }
    } else {
        let failed = chunk_processing::mark_chunk_failed_and_refresh_summary(db, chunk).await?;
        if failed > 0 {
            mq.quarantine_worker_message(
                &message.raw,
                &format!("worker message retry budget exhausted: {}", reason),
                error_class,
            )
            .await?;
            Ok(RetrySettlement::FailedExhausted)
        } else {
            mq.ack(&message.raw).await?;
            Ok(RetrySettlement::AcknowledgedStale)
        }
    }
}

async fn settle_chunk_waiting_for_execution_retry(
    db: &sqlx::PgPool,
    mq: &crate::mq::Client,
    message: &crate::mq::ConsumedMessage,
    chunk: &crate::models::run_chunk::RunChunk,
    reason: &str,
    next_retry_after: Option<chrono::DateTime<chrono::Utc>>,
) -> anyhow::Result<RetrySettlement> {
    // This settlement path is for planned execution retry waits. It releases
    // the chunk and delays redelivery without incrementing the worker-failure
    // retry count; the database retry_after field remains the source of truth.
    let released = chunk_processing::release_chunk_as_pending(db, chunk).await?;
    if released == 0 {
        mq.ack(&message.raw).await?;
        return Ok(RetrySettlement::AcknowledgedStale);
    }

    let delay_seconds = next_retry_after
        .map(|retry_after| {
            retry_after
                .signed_duration_since(chrono::Utc::now())
                .num_seconds()
                .max(1)
        })
        .unwrap_or(1);
    mq.delay_worker_message(&message.raw, delay_seconds, reason, "execution_retry")
        .await?;

    Ok(RetrySettlement::Delayed)
}

async fn run_worker_message(
    context: Context,
    evaluator_loader: &EvaluatorLoaderService,
    runtime: WorkerRuntime,
    message: crate::mq::ConsumedMessage,
) -> anyhow::Result<WorkerCycleOutcome> {
    // --- Acquire shared services ---
    // Settlement paths use the message queue handle to keep ack/retry behavior
    // centralized. The execution database is resolved after payload validation
    // because routing requires run_id + run_shard from the chunk-ready message.
    let mq = context.mq().await?;

    debug!(
        delivery_tag = message.delivery_tag(),
        "consumed worker message"
    );

    // --- Validate chunk-ready payload ---
    // Schema errors cannot map to a run chunk safely, so they go to quarantine
    // instead of retry.
    let payload = match serde_json::from_value::<ChunkReadyMessage>(message.payload.clone()) {
        Ok(payload) => payload,
        Err(err) => {
            mq.quarantine_worker_message(
                &message.raw,
                &format!("invalid chunk-ready message payload: {}", err),
                "invalid_schema",
            )
            .await?;
            warn!(error = %err, "quarantined invalid chunk-ready message payload");
            debug!(
                delivery_tag = message.delivery_tag(),
                "invalid message quarantined"
            );
            return Ok(WorkerCycleOutcome::Processed);
        }
    };

    debug!(
        run_id = %payload.run_id,
        run_shard = payload.run_shard,
        chunk_id = %payload.chunk_id,
        "parsed chunk-ready message payload"
    );

    // --- Resolve execution storage ---
    // Chunk leases, case batches, attempts, evaluator results, aggregates, and
    // chunk terminal state are execution-owned. Resolve this once at the worker
    // workflow boundary so lower helpers do not need to know topology.
    let db_service = context.db().await?;
    let route = match db_service
        .execution_route(payload.run_id, payload.run_shard)
        .await
    {
        Ok(route) => route,
        Err(err) if execution_route_is_temporarily_blocked(&err) => {
            mq.delay_worker_message(
                &message.raw,
                WORKER_ROUTE_RETRY_DELAY_SECONDS,
                &err.to_string(),
                "execution_route_blocked",
            )
            .await?;
            info!(
                run_id = %payload.run_id,
                run_shard = payload.run_shard,
                chunk_id = %payload.chunk_id,
                delay_seconds = WORKER_ROUTE_RETRY_DELAY_SECONDS,
                error = %err,
                "execution route is temporarily blocked; delayed worker message"
            );
            return Ok(WorkerCycleOutcome::Processed);
        }
        Err(err) => return Err(err),
    };
    let db = &route.database;

    // --- Claim chunk ownership ---
    // Duplicate, stale, cancelled, completed, or not-yet-running chunks are
    // acknowledged because the database refused ownership.
    let claim = chunk_processing::claim_routed_chunk_for_processing(
        db_service,
        &route,
        payload.run_id,
        payload.run_shard,
        payload.chunk_id,
        CHUNK_LEASE_SECONDS,
    )
    .await;
    let claimed = match claim {
        Ok(claimed) => claimed,
        Err(err) if execution_route_is_temporarily_blocked(&err) => {
            mq.delay_worker_message(
                &message.raw,
                WORKER_ROUTE_RETRY_DELAY_SECONDS,
                &err.to_string(),
                "execution_route_changed_before_claim",
            )
            .await?;
            info!(
                run_id = %payload.run_id,
                run_shard = payload.run_shard,
                chunk_id = %payload.chunk_id,
                delay_seconds = WORKER_ROUTE_RETRY_DELAY_SECONDS,
                error = %err,
                "execution route changed before chunk claim; delayed worker message"
            );
            return Ok(WorkerCycleOutcome::Processed);
        }
        Err(err) => return Err(err),
    };
    let Some(mut chunk) = claimed else {
        mq.ack(&message.raw).await?;
        info!(
            chunk_id = %payload.chunk_id,
            run_id = %payload.run_id,
            "chunk not claimable; acknowledging message"
        );
        debug!(
            delivery_tag = message.delivery_tag(),
            "unclaimable chunk message acknowledged"
        );
        return Ok(WorkerCycleOutcome::Processed);
    };

    debug!(
        run_id = %chunk.run_id,
        run_shard = chunk.run_shard,
        chunk_id = %chunk.id,
        ordinal_start = chunk.ordinal_start,
        ordinal_end = chunk.ordinal_end,
        "claimed chunk for processing"
    );

    // --- Load run context and extend lease ---
    // The opaque claim token remains the authority for later writes while the
    // heartbeat advances its expiration deadline.
    let run_context = evaluator_loader
        .get_or_build_run_context(chunk.run_id, chunk.run_shard, db)
        .await?;

    let Some(extended_chunk) =
        chunk_processing::extend_chunk_lease(db, &chunk, CHUNK_PROCESSING_LEASE_SECONDS).await?
    else {
        mq.ack(&message.raw).await?;
        warn!(
            run_id = %chunk.run_id,
            chunk_id = %chunk.id,
            lease_seconds = CHUNK_PROCESSING_LEASE_SECONDS,
            "chunk lease was lost before processing budget extension; acknowledged stale message"
        );
        return Ok(WorkerCycleOutcome::Processed);
    };
    chunk = extended_chunk;
    debug!(
        run_id = %chunk.run_id,
        chunk_id = %chunk.id,
        lease_seconds = CHUNK_PROCESSING_LEASE_SECONDS,
        leased_until = ?chunk.leased_until,
        "extended chunk lease for processing budget"
    );
    let heartbeat = ChunkHeartbeat::start(db, chunk.clone(), runtime.clone());
    let attempt_lease = execution_processing::AttemptLeaseContext {
        worker_id: runtime.worker_id,
        worker_host: runtime.worker_host.clone(),
        queue_message_id: Uuid::now_v7(),
        broker_message_id: broker_message_id(&message.raw),
        lease_seconds: ATTEMPT_LEASE_SECONDS,
    };

    // --- Warm evaluator components ---
    // Warmup failures are treated as recoverable worker failures because no
    // case state has been advanced yet.
    match evaluator_loader
        .warm_refs(&run_context.evaluator_refs)
        .await
    {
        Ok(stats) => {
            debug!(
                run_id = %chunk.run_id,
                requested = stats.requested,
                cache_hits = stats.cache_hits,
                loaded = stats.loaded,
                "completed evaluator warmup for run"
            );
        }
        Err(err) => {
            let chunk = heartbeat.stop().await;
            let settlement = settle_retryable_chunk_failure(
                db,
                mq,
                &message,
                &chunk,
                &err.to_string(),
                "evaluator_warmup",
            )
            .await?;
            warn!(
                run_id = %chunk.run_id,
                chunk_id = %chunk.id,
                error = %err,
                ?settlement,
                "failed evaluator warmup; handled claimed chunk lease"
            );
            return Ok(WorkerCycleOutcome::Processed);
        }
    }

    // --- Load chunk case batch ---
    // Loading failure is retryable as long as this worker still owns the chunk
    // lease.
    let batch_result = chunk_processing::load_chunk_case_batch(db, &chunk).await;
    match batch_result {
        Ok(cases) => {
            let mut succeeded = 0usize;
            let mut failed = 0usize;

            // --- Process due case executions ---
            // The execution workflow skips cases that are terminal or waiting
            // for retry_after.
            let processed = match execution_processing::process_case_batch_execution(
                &context,
                db,
                &chunk,
                &attempt_lease,
                &run_context.profile,
                &run_context.evaluator_catalog,
                &cases,
            )
            .await
            {
                Ok(processed) => processed,
                Err(err) => {
                    let chunk = heartbeat.stop().await;
                    let settlement = settle_retryable_chunk_failure(
                        db,
                        mq,
                        &message,
                        &chunk,
                        &err.to_string(),
                        "chunk_processing",
                    )
                    .await?;
                    warn!(
                        run_id = %chunk.run_id,
                        chunk_id = %chunk.id,
                        error = %err,
                        ?settlement,
                        "failed chunk case processing; handled claimed chunk lease"
                    );
                    return Ok(WorkerCycleOutcome::Processed);
                }
            };

            // --- Build terminal transition batch ---
            let mut terminal_transitions = Vec::with_capacity(processed.len());
            for processed in processed {
                let completed = processed.terminal_transition.completed;
                let failure_message = processed.terminal_transition.error_message.clone();
                terminal_transitions.push(processed.terminal_transition);

                if completed {
                    succeeded += 1;
                    debug!(
                        run_id = %chunk.run_id,
                        execution_id = %processed.execution_id,
                        attempt_id = %processed.attempt_id,
                        evaluator_result_count = processed.result_count,
                        "completed execution persistence for case"
                    );
                } else {
                    failed += 1;
                    warn!(
                        run_id = %chunk.run_id,
                        execution_id = %processed.execution_id,
                        attempt_id = %processed.attempt_id,
                        error = %failure_message.unwrap_or_else(|| "unknown failure".to_string()),
                        "case execution completed with terminal failure"
                    );
                }
            }

            // --- Apply guarded terminal transitions ---
            // Failures here are retryable worker failures because evidence may
            // have been written, but execution state was not advanced safely.
            if let Err(err) = execution_processing::finalize_execution_terminal_transitions(
                db,
                chunk.run_id,
                chunk.run_shard,
                runtime.worker_id,
                i32::try_from(run_context.profile.defaults.max_attempts)?,
                &terminal_transitions,
            )
            .await
            {
                let chunk = heartbeat.stop().await;
                let settlement = settle_retryable_chunk_failure(
                    db,
                    mq,
                    &message,
                    &chunk,
                    &err.to_string(),
                    "terminal_transition",
                )
                .await?;
                warn!(
                    run_id = %chunk.run_id,
                    chunk_id = %chunk.id,
                    error = %err,
                    ?settlement,
                    "failed chunk-level execution terminal transitions; handled claimed chunk lease"
                );
                return Ok(WorkerCycleOutcome::Processed);
            }

            // --- Check for pending execution retries ---
            // Open retry work releases the chunk and delays the message until
            // the next retry window.
            let chunk_state = execution_processing::summarize_chunk_execution_state(
                db,
                chunk.run_id,
                chunk.run_shard,
                &cases,
            )
            .await?;
            if chunk_state.open_execution_count > 0 {
                let chunk = heartbeat.stop().await;
                let reason = format!(
                    "chunk has {} open executions, including {} retry-scheduled executions; next_retry_after={:?}",
                    chunk_state.open_execution_count,
                    chunk_state.retry_scheduled_count,
                    chunk_state.next_retry_after
                );
                let settlement = settle_chunk_waiting_for_execution_retry(
                    db,
                    mq,
                    &message,
                    &chunk,
                    &reason,
                    chunk_state.next_retry_after,
                )
                .await?;
                info!(
                    run_id = %chunk.run_id,
                    chunk_id = %chunk.id,
                    open_execution_count = chunk_state.open_execution_count,
                    retry_scheduled_count = chunk_state.retry_scheduled_count,
                    next_retry_after = ?chunk_state.next_retry_after,
                    ?settlement,
                    "chunk still has open execution retry work; handled claimed chunk lease"
                );
                return Ok(WorkerCycleOutcome::Processed);
            }

            // --- Complete chunk and acknowledge message ---
            // All executions in the chunk are terminal, so complete the chunk
            // under the same lease token.
            let chunk = heartbeat.stop().await;
            let completed =
                chunk_processing::mark_chunk_completed_and_refresh_summary(db, &chunk).await?;
            if completed == 0 {
                mq.ack(&message.raw).await?;
                warn!(
                    run_id = %chunk.run_id,
                    chunk_id = %chunk.id,
                    "chunk lease was no longer owned at completion; acknowledged stale message"
                );
                return Ok(WorkerCycleOutcome::Processed);
            }
            mq.ack(&message.raw).await?;
            info!(
                run_id = %chunk.run_id,
                chunk_id = %chunk.id,
                case_count = cases.len(),
                cases_succeeded = succeeded,
                cases_failed = failed,
                "processed case batch from queue chunk"
            );
            debug!(
                delivery_tag = message.delivery_tag(),
                chunk_id = %chunk.id,
                "chunk processing complete; message acknowledged"
            );
            Ok(WorkerCycleOutcome::Processed)
        }
        Err(err) => {
            let chunk = heartbeat.stop().await;
            let settlement = settle_retryable_chunk_failure(
                db,
                mq,
                &message,
                &chunk,
                &err.to_string(),
                "case_batch_load",
            )
            .await?;
            warn!(
                run_id = %chunk.run_id,
                chunk_id = %chunk.id,
                error = %err,
                ?settlement,
                "failed to load chunk case batch; handled claimed chunk lease"
            );
            debug!(
                delivery_tag = message.delivery_tag(),
                chunk_id = %chunk.id,
                ?settlement,
                "chunk processing failed; handled claimed chunk lease"
            );
            Ok(WorkerCycleOutcome::Processed)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        ChunkReadyMessage,
        DEFAULT_WORKER_MAX_INFLIGHT_CHUNKS,
        execution_route_is_temporarily_blocked,
        worker_stream_prefetch,
    };
    use crate::context::database::ExecutionRouteError;

    #[test]
    fn default_worker_processes_one_chunk_at_a_time() {
        assert_eq!(DEFAULT_WORKER_MAX_INFLIGHT_CHUNKS, 1);
        assert_eq!(
            worker_stream_prefetch(DEFAULT_WORKER_MAX_INFLIGHT_CHUNKS),
            1
        );
    }

    #[test]
    fn worker_stream_prefetch_matches_inflight_chunk_capacity() {
        assert_eq!(worker_stream_prefetch(1), 1);
        assert_eq!(worker_stream_prefetch(4), 4);
        assert_eq!(worker_stream_prefetch(64), 64);
    }

    #[test]
    fn chunk_ready_message_carries_execution_route_key() {
        let run_id = Uuid::now_v7();
        let chunk_id = Uuid::now_v7();

        let message: ChunkReadyMessage = serde_json::from_value(json!({
            "run_id": run_id,
            "run_shard": 42,
            "chunk_id": chunk_id,
        }))
        .unwrap();

        assert_eq!(message.run_id, run_id);
        assert_eq!(message.run_shard, 42);
        assert_eq!(message.chunk_id, chunk_id);
    }

    #[test]
    fn non_dispatchable_execution_route_is_temporary_worker_backpressure() {
        let error = anyhow::Error::new(ExecutionRouteError::NonDispatchableShardPlacement {
            run_id: Uuid::now_v7(),
            run_shard: 7,
            status: "moving".to_string(),
        });

        assert!(execution_route_is_temporarily_blocked(&error));
    }

    #[test]
    fn missing_execution_route_is_not_temporary_worker_backpressure() {
        let error = anyhow::Error::new(ExecutionRouteError::MissingShardPlacement {
            run_id: Uuid::now_v7(),
            run_shard: 7,
        });

        assert!(!execution_route_is_temporarily_blocked(&error));
    }

    #[test]
    fn stale_execution_route_is_temporary_worker_backpressure() {
        let error = anyhow::Error::new(ExecutionRouteError::StaleExecutionRoute {
            run_id: Uuid::now_v7(),
            run_shard: 7,
            expected_database_alias: "primary".to_string(),
            expected_route_version: 1,
            actual_database_alias: "primary".to_string(),
            actual_status: "moving".to_string(),
            actual_route_version: 2,
        });

        assert!(execution_route_is_temporarily_blocked(&error));
    }
}
