use super::*;

pub(super) async fn exec(
    context: Context,
    evaluator: String,
    state: EvaluatorState,
    state_reason: Option<String>,
) -> anyhow::Result<()> {
    info!("setting evaluator state {} -> {:?}", evaluator, state);

    let db = context.db().await?;
    let evaluator = parse_fully_qualified_evaluator(&evaluator)?;

    // todo - handle failure reason (e.g. removed -> active failure)
    let affected = evaluators::update_evaluator_state(
        db,
        &evaluator.namespace,
        &evaluator.name,
        &evaluator.version,
        &EvaluatorPatch {
            state: state.clone(),
            state_reason,
        },
    )
    .await?;

    if affected == 0 {
        anyhow::bail!(
            "failed to set evaluator state {}/{}:{} -> {:?}",
            evaluator.namespace,
            evaluator.name,
            evaluator.version,
            state,
        );
    } else {
        info!(
            "set evaluator state {}/{}:{} -> {:?}",
            evaluator.namespace, evaluator.name, evaluator.version, state,
        );
    }

    Ok(())
}
