//! Evaluator test command implementation.
//!
//! Runs one registry evaluator against canonical evaluator `input` JSON and
//! returns the evaluator `output` plus normalized findings. Disabled or removed
//! evaluators cannot be tested, and test input must match the host-side
//! evaluator execution contract.

use super::*;

pub(super) async fn exec(
    context: Context,
    evaluator: String,
    input: Option<String>,
    input_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    info!("testing evaluator {}", evaluator);

    let db = context.dbr().await?.control().await?;
    let out = context.out().await?;
    let wasm = context.wasm().await?;
    let evaluator = parse_fully_qualified_evaluator(&evaluator)?;

    let evaluator_record = evaluators::select_evaluator(
        db,
        &evaluator.namespace,
        &evaluator.name,
        &evaluator.version,
    )
    .await?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "evaluator not found: {}/{}:{}",
            evaluator.namespace,
            evaluator.name,
            evaluator.version,
        )
    })?;

    match evaluator_record.state {
        EvaluatorState::Disabled | EvaluatorState::Removed => {
            anyhow::bail!(
                "evaluator {}/{}:{} cannot be tested while in state '{}'",
                evaluator_record.namespace,
                evaluator_record.name,
                evaluator_record.version,
                serde_json::to_string(&evaluator_record.state)?.trim_matches('"'),
            );
        }
        _ => {}
    }

    let input_raw = match (input, input_file) {
        (Some(raw), None) => raw,
        (None, Some(path)) => fs::read_to_string(path)?,
        _ => anyhow::bail!("exactly one of --input or --input-file must be provided"),
    };

    let parsed_input: EvaluatorInput = serde_json::from_str(&input_raw)
        .map_err(|err| anyhow::anyhow!("invalid evaluator test input json: {}", err))?;

    let evaluation_output = wasm.test_evaluator(&evaluator_record.wasm_bytes, parsed_input)?;

    let normalized_results = evaluation_output.clone().normalize();

    let payload = json!({
        "data": {
            "namespace": evaluator_record.namespace,
            "name": evaluator_record.name,
            "version": evaluator_record.version,
            "state": evaluator_record.state,
            "output": evaluation_output,
            "normalized_results": normalized_results,
        }
    });

    out.write_value(&payload)?;

    Ok(())
}
