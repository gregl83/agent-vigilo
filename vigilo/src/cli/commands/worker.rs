//! Worker process command.
//!
//! Workers consume chunk-ready queue messages, claim work, warm evaluator
//! components, execute case-level evaluation workflows, and acknowledge or
//! requeue queue messages based on outcome.

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
use tokio::task::JoinSet;
use tracing::{
    debug,
    info,
    warn,
};
use uuid::Uuid;

use super::Executable;
use crate::{
    context::Context,
    contracts::run::RunProfile,
    db::{
        tables::runs,
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
const CHUNK_LEASE_SAFETY_SECONDS: i32 = 120;
const CHUNK_LEASE_EVALUATOR_BUDGET_SECONDS_PER_CASE_BATCH: i32 = 30;
const MAX_COMPUTED_CHUNK_LEASE_SECONDS: i32 = 86_400;
const RUN_CONTEXT_CACHE_MAX_ENTRIES: u64 = 1024;
const RUN_CONTEXT_CACHE_TTI_SECONDS: u64 = 900;
const WARMUP_PARALLELISM: usize = 8;
const WORKER_STREAM_PREFETCH: u16 = 64;
const CASE_EXECUTION_PARALLELISM_FOR_LEASE_BUDGET: i32 = 8;

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

#[derive(Debug, Deserialize)]
/// Queue payload that signals a run chunk is ready for processing.
struct ChunkReadyMessage {
    run_id: Uuid,
    chunk_id: Uuid,
}

#[derive(Debug, Default, Clone, Copy)]
/// Summary of evaluator cache warmup performed before processing a chunk.
struct WarmupStats {
    requested: usize,
    cache_hits: usize,
    loaded: usize,
}

fn compute_chunk_processing_lease_seconds(
    profile: &RunProfile,
    chunk: &crate::models::run_chunk::RunChunk,
) -> i32 {
    let case_count = (chunk.ordinal_end - chunk.ordinal_start).max(1);
    let case_batches = (case_count + CASE_EXECUTION_PARALLELISM_FOR_LEASE_BUDGET - 1)
        / CASE_EXECUTION_PARALLELISM_FOR_LEASE_BUDGET;
    let request_timeout = i32::try_from(profile.defaults.request_timeout_secs)
        .unwrap_or(MAX_COMPUTED_CHUNK_LEASE_SECONDS);
    let per_batch_budget =
        request_timeout.saturating_add(CHUNK_LEASE_EVALUATOR_BUDGET_SECONDS_PER_CASE_BATCH);
    let computed = case_batches
        .saturating_mul(per_batch_budget)
        .saturating_add(CHUNK_LEASE_SAFETY_SECONDS);

    computed.clamp(CHUNK_LEASE_SECONDS, MAX_COMPUTED_CHUNK_LEASE_SECONDS)
}

#[derive(Clone)]
/// Worker-local helper that resolves evaluator components into runtime cache.
///
/// This service is intentionally lightweight and command-scoped. It validates
/// evaluator identifiers and state before compiling WASM bytes into cached
/// Wasmtime components.
struct EvaluatorLoaderService {
    context: Context,
    run_context_cache: Arc<Cache<Uuid, Arc<WorkerRunContext>>>,
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
    ) -> anyhow::Result<Arc<WorkerRunContext>> {
        let context = self.context.clone();

        let context_result = self
            .run_context_cache
            .try_get_with::<_, anyhow::Error>(run_id, async move {
                let db = context.db().await?;
                let Some(profile_snapshot) =
                    runs::select_run_profile_snapshot_by_id(db, run_id).await?
                else {
                    anyhow::bail!("run '{}' missing profile snapshot", run_id);
                };
                let profile: RunProfile =
                    serde_json::from_value(profile_snapshot).map_err(|err| {
                        anyhow::anyhow!("run '{}' profile is invalid: {}", run_id, err)
                    })?;
                let evaluator_refs = execution_processing::evaluator_refs_from_profile(&profile)?;
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
    Start,

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
            Some(SubCommand::Start) => {
                info!("starting worker process");
                start::exec(context).await
            }
            Some(SubCommand::Once) => {
                info!("running single worker cycle");
                once::exec(context).await
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
    max_messages: usize,
) -> anyhow::Result<usize> {
    let mut processed = 0usize;

    for _ in 0..max_messages {
        match run_worker_cycle(context.clone(), evaluator_loader).await? {
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
) -> anyhow::Result<WorkerCycleOutcome> {
    debug!("starting worker cycle pre-flight");

    debug!("acquiring messaging context");
    let mq = context.mq().await?;
    debug!("messaging context ready");

    debug!("attempting to consume worker message");

    let Some(raw_message) = mq.consume_worker_message().await? else {
        debug!("no worker messages available");
        return Ok(WorkerCycleOutcome::Empty);
    };
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

    run_worker_message(context, evaluator_loader, message).await
}

async fn settle_retryable_chunk_failure(
    db: &sqlx::PgPool,
    mq: &crate::mq::Client,
    message: &crate::mq::ConsumedMessage,
    chunk: &crate::models::run_chunk::RunChunk,
    reason: &str,
    error_class: &str,
) -> anyhow::Result<RetrySettlement> {
    if mq.can_retry_worker_message(&message.raw) {
        let released = chunk_processing::release_chunk_as_pending(db, chunk).await?;
        if released > 0 {
            mq.retry_worker_message(&message.raw, reason, error_class)
                .await?;
            Ok(RetrySettlement::Retried)
        } else {
            mq.ack(message.delivery_tag()).await?;
            Ok(RetrySettlement::AcknowledgedStale)
        }
    } else {
        let failed = chunk_processing::mark_chunk_failed(db, chunk).await?;
        if failed > 0 {
            mq.quarantine_worker_message(
                &message.raw,
                &format!("worker message retry budget exhausted: {}", reason),
                error_class,
            )
            .await?;
            Ok(RetrySettlement::FailedExhausted)
        } else {
            mq.ack(message.delivery_tag()).await?;
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
    let released = chunk_processing::release_chunk_as_pending(db, chunk).await?;
    if released == 0 {
        mq.ack(message.delivery_tag()).await?;
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
    message: crate::mq::ConsumedMessage,
) -> anyhow::Result<WorkerCycleOutcome> {
    let db = context.db().await?;
    let mq = context.mq().await?;

    debug!(
        delivery_tag = message.delivery_tag(),
        "consumed worker message"
    );

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
        chunk_id = %payload.chunk_id,
        "parsed chunk-ready message payload"
    );

    let Some(mut chunk) = chunk_processing::claim_chunk_for_processing(
        db,
        payload.run_id,
        payload.chunk_id,
        CHUNK_LEASE_SECONDS,
    )
    .await?
    else {
        mq.ack(message.delivery_tag()).await?;
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
        chunk_id = %chunk.id,
        ordinal_start = chunk.ordinal_start,
        ordinal_end = chunk.ordinal_end,
        "claimed chunk for processing"
    );

    let run_context = evaluator_loader
        .get_or_build_run_context(chunk.run_id)
        .await?;

    let processing_lease_seconds =
        compute_chunk_processing_lease_seconds(&run_context.profile, &chunk);
    let Some(extended_chunk) =
        chunk_processing::extend_chunk_lease(db, &chunk, processing_lease_seconds).await?
    else {
        mq.ack(message.delivery_tag()).await?;
        warn!(
            run_id = %chunk.run_id,
            chunk_id = %chunk.id,
            lease_seconds = processing_lease_seconds,
            "chunk lease was lost before processing budget extension; acknowledged stale message"
        );
        return Ok(WorkerCycleOutcome::Processed);
    };
    chunk = extended_chunk;
    debug!(
        run_id = %chunk.run_id,
        chunk_id = %chunk.id,
        lease_seconds = processing_lease_seconds,
        leased_until = ?chunk.leased_until,
        "extended chunk lease for processing budget"
    );

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

    let batch_result = chunk_processing::load_chunk_case_batch(db, &chunk).await;
    match batch_result {
        Ok(cases) => {
            let mut succeeded = 0usize;
            let mut failed = 0usize;

            let processed = match execution_processing::process_case_batch_execution(
                &context,
                db,
                chunk.run_id,
                &run_context.profile,
                &run_context.evaluator_catalog,
                &cases,
            )
            .await
            {
                Ok(processed) => processed,
                Err(err) => {
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

            if let Err(err) = execution_processing::finalize_execution_terminal_transitions(
                db,
                chunk.run_id,
                i32::try_from(run_context.profile.defaults.max_attempts)?,
                &terminal_transitions,
            )
            .await
            {
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

            let chunk_state =
                execution_processing::summarize_chunk_execution_state(db, chunk.run_id, &cases)
                    .await?;
            if chunk_state.open_execution_count > 0 {
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

            let completed = chunk_processing::mark_chunk_completed(db, &chunk).await?;
            if completed == 0 {
                mq.ack(message.delivery_tag()).await?;
                warn!(
                    run_id = %chunk.run_id,
                    chunk_id = %chunk.id,
                    "chunk lease was no longer owned at completion; acknowledged stale message"
                );
                return Ok(WorkerCycleOutcome::Processed);
            }

            mq.ack(message.delivery_tag()).await?;
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
