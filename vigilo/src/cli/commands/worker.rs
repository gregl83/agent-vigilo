use std::time::Duration;

use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use serde::Deserialize;
use tracing::{
    debug,
    info,
    warn,
};
use uuid::Uuid;

use super::Executable;
use crate::{
    context::Context,
    db::{
        tables::{
            evaluators,
            runs,
        },
        workflows::{
            chunk_processing,
            execution_processing,
        },
    },
    runtime::ServiceRunner,
};

const WORKER_TICK_SECONDS: u64 = 5;
const CHUNK_LEASE_SECONDS: i32 = 60;

#[derive(Debug, Deserialize)]
struct ChunkReadyMessage {
    run_id: Uuid,
    chunk_id: Uuid,
}

#[derive(Debug, Default, Clone, Copy)]
struct WarmupStats {
    requested: usize,
    cache_hits: usize,
    loaded: usize,
}

#[derive(Clone)]
struct EvaluatorLoaderService {
    context: Context,
}

impl EvaluatorLoaderService {
    fn new(context: Context) -> Self {
        Self { context }
    }

    async fn get_or_load(
        &self,
        evaluator_ref: &str,
    ) -> anyhow::Result<wasmtime::component::Component> {
        let cache = self.context.reg().await?;
        if let Some(component) = cache.get(evaluator_ref).await {
            return Ok(component);
        }

        let identity =
            crate::contracts::evaluator_ref::parse_fully_qualified_evaluator(evaluator_ref)?;
        let db = self.context.db().await?;
        let evaluator_record = evaluators::select_evaluator(
            db,
            &identity.namespace,
            &identity.name,
            &identity.version,
        )
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "evaluator '{}' was not found in registry during worker warmup",
                evaluator_ref
            )
        })?;

        if !execution_processing::is_runnable_evaluator_state(&evaluator_record.state) {
            anyhow::bail!(
                "evaluator '{}' is not runnable in state '{:?}'",
                evaluator_ref,
                evaluator_record.state
            );
        }

        let wasm = self.context.wasm().await?;
        let component = wasm.compile_component(&evaluator_record.wasm_bytes)?;
        cache
            .insert(evaluator_ref.to_string(), component.clone())
            .await;
        Ok(component)
    }

    async fn warm_refs(&self, evaluator_refs: &[String]) -> anyhow::Result<WarmupStats> {
        let mut stats = WarmupStats {
            requested: evaluator_refs.len(),
            ..WarmupStats::default()
        };

        let cache = self.context.reg().await?;
        for evaluator_ref in evaluator_refs {
            if cache.get(evaluator_ref).await.is_some() {
                stats.cache_hits += 1;
                continue;
            }

            self.get_or_load(evaluator_ref).await?;
            stats.loaded += 1;
        }

        Ok(stats)
    }

    async fn warm_run_evaluators(&self, run_id: Uuid) -> anyhow::Result<WarmupStats> {
        let db = self.context.db().await?;
        let run = runs::select_run_by_id(db, run_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("run '{}' not found during evaluator warmup", run_id))?;

        let evaluator_refs =
            execution_processing::evaluator_refs_from_snapshot(&run.config_snapshot)?;
        self.warm_refs(&evaluator_refs).await
    }
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
    let evaluator_loader = EvaluatorLoaderService::new(context.clone());

    debug!("starting worker cycle pre-flight");

    debug!("acquiring database context");
    let db = context.db().await?;
    debug!("database context ready");

    debug!("acquiring messaging context");
    let mq = context.mq().await?;
    debug!("messaging context ready");

    debug!("attempting to consume worker message");

    let Some(message) = mq.consume_worker_message().await? else {
        info!("no worker messages available");
        debug!("worker cycle complete with no message");
        return Ok(());
    };

    debug!(
        delivery_tag = message.delivery_tag,
        "consumed worker message"
    );

    let payload = match serde_json::from_value::<ChunkReadyMessage>(message.payload.clone()) {
        Ok(payload) => payload,
        Err(err) => {
            mq.ack(message.delivery_tag).await?;
            warn!(error = %err, "dropping invalid chunk-ready message payload");
            debug!(
                delivery_tag = message.delivery_tag,
                "invalid message acknowledged and dropped"
            );
            return Ok(());
        }
    };

    debug!(
        run_id = %payload.run_id,
        chunk_id = %payload.chunk_id,
        "parsed chunk-ready message payload"
    );

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
        debug!(
            delivery_tag = message.delivery_tag,
            "unclaimable chunk message acknowledged"
        );
        return Ok(());
    };

    debug!(
        run_id = %chunk.run_id,
        chunk_id = %chunk.id,
        ordinal_start = chunk.ordinal_start,
        ordinal_end = chunk.ordinal_end,
        "claimed chunk for processing"
    );

    match evaluator_loader.warm_run_evaluators(chunk.run_id).await {
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
            chunk_processing::release_chunk_as_pending(db, chunk.id).await?;
            mq.nack_requeue(message.delivery_tag).await?;
            warn!(
                run_id = %chunk.run_id,
                chunk_id = %chunk.id,
                error = %err,
                "failed evaluator warmup; chunk released and message requeued"
            );
            return Ok(());
        }
    }

    let run = runs::select_run_by_id(db, chunk.run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run '{}' missing while processing chunk", chunk.run_id))?;
    let run_profile = execution_processing::run_profile_from_snapshot(&run.config_snapshot)?;

    let batch_result = chunk_processing::load_chunk_case_batch(db, &chunk).await;
    match batch_result {
        Ok(cases) => {
            let mut succeeded = 0usize;
            let mut failed = 0usize;

            for case in &cases {
                let processing_result = execution_processing::process_case_execution(
                    &context,
                    db,
                    chunk.run_id,
                    &run_profile,
                    case,
                )
                .await;

                match processing_result {
                    Ok(processed) => {
                        succeeded += 1;
                        debug!(
                            run_id = %chunk.run_id,
                            case_id = %case.case_id,
                            execution_id = %processed.execution_id,
                            attempt_id = %processed.attempt_id,
                            evaluator_result_count = processed.result_count,
                            "completed execution persistence for case"
                        );
                    }
                    Err(err) => {
                        failed += 1;
                        warn!(
                            run_id = %chunk.run_id,
                            case_id = %case.case_id,
                            error = %err,
                            "failed processing case execution; recorded as failed when possible"
                        );
                    }
                }
            }

            chunk_processing::mark_chunk_completed(db, chunk.id).await?;
            mq.ack(message.delivery_tag).await?;
            info!(
                run_id = %chunk.run_id,
                chunk_id = %chunk.id,
                case_count = cases.len(),
                cases_succeeded = succeeded,
                cases_failed = failed,
                "processed case batch from queue chunk"
            );
            debug!(
                delivery_tag = message.delivery_tag,
                chunk_id = %chunk.id,
                "chunk processing complete; message acknowledged"
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
            debug!(
                delivery_tag = message.delivery_tag,
                chunk_id = %chunk.id,
                "chunk processing failed; message requeued"
            );
            Ok(())
        }
    }
}
