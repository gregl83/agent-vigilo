//! Single-pass worker mode.
//!
//! Used by `vigilo worker once` to process at most one worker message and exit.
//! This mode is useful for local debugging, smoke tests, and externally
//! scheduled workers; all chunk claiming and settlement rules stay in the
//! parent worker module.

use super::*;

/// Processes one worker cycle and exits.
pub(super) async fn exec(context: Context, runtime: WorkerRuntime) -> anyhow::Result<()> {
    let evaluator_loader = EvaluatorLoaderService::new(context.clone());
    run_worker_drain_pass(context, &evaluator_loader, runtime, 1).await?;
    Ok(())
}
