use std::{
    collections::BTreeSet,
    time::Duration,
};

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
    contracts::run::RunProfile,
    db::{
        tables::{
            evaluators,
            runs,
        },
        workflows::chunk_processing,
    },
    models::evaluator::EvaluatorState,
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

    async fn invalidate(&self, evaluator_ref: &str) -> anyhow::Result<()> {
        let cache = self.context.reg().await?;
        cache.invalidate(evaluator_ref).await;
        Ok(())
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

        if !is_runnable_evaluator_state(&evaluator_record.state) {
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

        let evaluator_refs = evaluator_refs_from_snapshot(&run.config_snapshot)?;
        self.warm_refs(&evaluator_refs).await
    }
}

fn evaluator_refs_from_snapshot(snapshot: &serde_json::Value) -> anyhow::Result<Vec<String>> {
    let profile_value = snapshot
        .get("profile")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("run snapshot is missing 'profile' payload"))?;

    let profile: RunProfile = serde_json::from_value(profile_value)
        .map_err(|err| anyhow::anyhow!("run snapshot profile is invalid: {}", err))?;

    let mut unique_refs = BTreeSet::new();
    for group in profile.case_groups {
        for binding in group.evaluators {
            unique_refs.insert(binding.evaluator_ref);
        }
    }

    Ok(unique_refs.into_iter().collect())
}

fn is_runnable_evaluator_state(state: &EvaluatorState) -> bool {
    matches!(
        state,
        EvaluatorState::Active | EvaluatorState::Deprecated | EvaluatorState::Yanked
    )
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
