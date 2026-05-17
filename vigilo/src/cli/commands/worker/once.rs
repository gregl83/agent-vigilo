use super::*;

/// Processes one worker cycle and exits.
pub(super) async fn exec(context: Context) -> anyhow::Result<()> {
    let evaluator_loader = EvaluatorLoaderService::new(context.clone());
    run_worker_drain_pass(context, &evaluator_loader, 1).await?;
    Ok(())
}
