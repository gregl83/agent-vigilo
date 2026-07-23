//! Run export command implementation.
//!
//! Exports run summary data and execution artifacts as either one JSON document
//! or streamed JSONL records. Use JSON for small runs and JSONL for large runs;
//! pagination must stay ordered by `(run_shard, execution_id)` so exports are
//! deterministic and resumable internally.

use super::*;

pub(super) fn run_export_payload(
    run: &Run,
    summary: &RunResultsSummary,
    executions: &[Execution],
    attempts: &[ExecutionAttempt],
    aggregates: &[ExecutionAggregate],
    evaluator_results: &[EvaluatorResult],
) -> Value {
    let mut attempts_by_execution: BTreeMap<(i16, Uuid), Vec<&ExecutionAttempt>> = BTreeMap::new();
    for attempt in attempts {
        attempts_by_execution
            .entry((attempt.run_shard, attempt.execution_id))
            .or_default()
            .push(attempt);
    }

    let mut aggregates_by_execution: BTreeMap<(i16, Uuid), &ExecutionAggregate> = BTreeMap::new();
    for aggregate in aggregates {
        aggregates_by_execution.insert((aggregate.run_shard, aggregate.execution_id), aggregate);
    }

    let mut results_by_attempt: BTreeMap<(i16, Uuid), Vec<&EvaluatorResult>> = BTreeMap::new();
    for result in evaluator_results {
        results_by_attempt
            .entry((result.run_shard, result.attempt_id))
            .or_default()
            .push(result);
    }

    let exported_executions = executions
        .iter()
        .map(|execution| {
            let exported_attempts = attempts_by_execution
                .get(&(execution.run_shard, execution.id))
                .into_iter()
                .flatten()
                .map(|attempt| {
                    let attempt_results = results_by_attempt
                        .get(&(attempt.run_shard, attempt.id))
                        .into_iter()
                        .flatten()
                        .map(|result| json!(result))
                        .collect::<Vec<_>>();

                    json!({
                        "attempt": attempt,
                        "evaluator_results": attempt_results,
                    })
                })
                .collect::<Vec<_>>();

            let aggregate = aggregates_by_execution
                .get(&(execution.run_shard, execution.id))
                .map(|row| json!(row))
                .unwrap_or(Value::Null);

            json!({
                "execution": execution,
                "aggregate": aggregate,
                "attempts": exported_attempts,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "data": {
            "run": {
                "run_id": run.id,
                "run_key": run.run_key,
                "status": run.status,
                "gate_status": run.gate_status,
                "expected_execution_count": run.expected_execution_count,
                "terminal_execution_count": run.terminal_execution_count,
                "passed_execution_count": run.passed_execution_count,
                "failed_execution_count": run.failed_execution_count,
                "errored_execution_count": run.errored_execution_count,
                "summary": run.summary,
                "error_message": run.error_message,
                "created_at": run.created_at,
                "completed_at": run.completed_at,
                "updated_at": run.updated_at,
            },
            "results": {
                "execution_count": summary.execution_count,
                "aggregate_count": summary.aggregate_count,
                "missing_aggregate_count": summary.missing_aggregate_count,
                "status_counts": {
                    "passed": summary.passed_execution_count,
                    "failed": summary.failed_execution_count,
                    "error": summary.error_execution_count,
                    "skipped": summary.skipped_execution_count,
                },
                "score": {
                    "average": summary.average_score,
                    "min": summary.min_score,
                    "max": summary.max_score,
                },
                "evaluator_result_count": summary.evaluator_result_count,
                "blocking_failure_count": summary.blocking_failure_count,
            },
            "executions": exported_executions,
        },
        "meta": {
            "summary_only": false,
            "execution_count": executions.len(),
            "attempt_count": attempts.len(),
            "aggregate_count": aggregates.len(),
            "evaluator_result_count": evaluator_results.len(),
        }
    })
}

pub(super) async fn exec(
    context: Context,
    run_id: String,
    format: RunExportFormat,
    batch_size: i64,
) -> anyhow::Result<()> {
    // --- Load export header ---
    // Validate options and load the run-level export header data.
    if batch_size <= 0 {
        anyhow::bail!("export batch_size must be greater than zero");
    }

    let run_id = parse_run_id(&run_id)?;
    let database_router = context.dbr().await?;
    let db = database_router.control().await?;
    let out = context.out().await?;

    let run = select_existing_run(db, run_id).await?;
    let summary = run_results_workflow::select_run_results_summary(database_router, run_id).await?;
    let routes = run_export_workflow::select_run_export_routes(database_router, run_id).await?;

    match format {
        RunExportFormat::Json => {
            // --- Materialize JSON export ---
            // JSON export loads the whole run into memory so callers receive
            // one structured document.
            let mut all_executions = Vec::new();
            let mut all_attempts = Vec::new();
            let mut all_aggregates = Vec::new();
            let mut all_evaluator_results = Vec::new();

            for route in &routes {
                let mut cursor: Option<Uuid> = None;
                loop {
                    let execution_batch = run_export_workflow::select_execution_batch(
                        route, run_id, cursor, batch_size,
                    )
                    .await?;
                    if execution_batch.is_empty() {
                        break;
                    }

                    cursor = execution_batch.last().map(|execution| execution.id);
                    let batch = run_export_workflow::select_batch_for_executions(
                        route,
                        run_id,
                        execution_batch,
                    )
                    .await?;

                    all_executions.extend(batch.executions);
                    all_attempts.extend(batch.attempts);
                    all_aggregates.extend(batch.aggregates);
                    all_evaluator_results.extend(batch.evaluator_results);
                }
            }

            let payload = run_export_payload(
                &run,
                &summary,
                &all_executions,
                &all_attempts,
                &all_aggregates,
                &all_evaluator_results,
            );
            out.write_value(&payload)?;
        }
        RunExportFormat::Jsonl => {
            // --- Stream JSONL header ---
            // JSONL export writes header records first, then emits
            // execution-scoped records batch by batch for large runs.
            let run_line = json!({
                "type": "run",
                "run": run,
            });
            out.write_line(serde_json::to_string(&run_line)?)?;

            let summary_line = json!({
                "type": "results_summary",
                "run_id": run_id,
                "results": {
                    "execution_count": summary.execution_count,
                    "aggregate_count": summary.aggregate_count,
                    "passed_execution_count": summary.passed_execution_count,
                    "failed_execution_count": summary.failed_execution_count,
                    "error_execution_count": summary.error_execution_count,
                    "skipped_execution_count": summary.skipped_execution_count,
                    "missing_aggregate_count": summary.missing_aggregate_count,
                    "evaluator_result_count": summary.evaluator_result_count,
                    "blocking_failure_count": summary.blocking_failure_count,
                    "average_score": summary.average_score,
                    "min_score": summary.min_score,
                    "max_score": summary.max_score,
                },
            });
            out.write_line(serde_json::to_string(&summary_line)?)?;

            for route in &routes {
                let route_line = json!({
                    "type": "execution_route",
                    "run_id": run_id,
                    "run_shard": route.run_shard(),
                    "database_alias": route.database_alias(),
                });
                out.write_line(serde_json::to_string(&route_line)?)?;

                let mut cursor: Option<Uuid> = None;
                loop {
                    // --- Load execution page ---
                    // Page executions by id and load child attempts,
                    // aggregates, and evaluator results only for that page.
                    let execution_batch = run_export_workflow::select_execution_batch(
                        route, run_id, cursor, batch_size,
                    )
                    .await?;
                    if execution_batch.is_empty() {
                        break;
                    }

                    cursor = execution_batch.last().map(|execution| execution.id);
                    let batch = run_export_workflow::select_batch_for_executions(
                        route,
                        run_id,
                        execution_batch,
                    )
                    .await?;

                    let mut attempts_by_execution: BTreeMap<(i16, Uuid), Vec<&ExecutionAttempt>> =
                        BTreeMap::new();
                    for attempt in &batch.attempts {
                        attempts_by_execution
                            .entry((attempt.run_shard, attempt.execution_id))
                            .or_default()
                            .push(attempt);
                    }

                    let mut aggregates_by_execution: BTreeMap<(i16, Uuid), &ExecutionAggregate> =
                        BTreeMap::new();
                    for aggregate in &batch.aggregates {
                        aggregates_by_execution
                            .insert((aggregate.run_shard, aggregate.execution_id), aggregate);
                    }

                    let mut results_by_attempt: BTreeMap<(i16, Uuid), Vec<&EvaluatorResult>> =
                        BTreeMap::new();
                    for result in &batch.evaluator_results {
                        results_by_attempt
                            .entry((result.run_shard, result.attempt_id))
                            .or_default()
                            .push(result);
                    }

                    // --- Emit execution group ---
                    // Emit one execution record followed by its aggregate,
                    // attempts, and attempt result rows. This preserves a
                    // stable local grouping without holding the entire export
                    // in memory.
                    for execution in &batch.executions {
                        let execution_line = json!({
                            "type": "execution",
                            "run_id": run_id,
                            "run_shard": route.run_shard(),
                            "database_alias": route.database_alias(),
                            "execution": execution,
                        });
                        out.write_line(serde_json::to_string(&execution_line)?)?;

                        if let Some(aggregate) =
                            aggregates_by_execution.get(&(execution.run_shard, execution.id))
                        {
                            let aggregate_line = json!({
                                "type": "execution_aggregate",
                                "run_id": run_id,
                                "run_shard": route.run_shard(),
                                "database_alias": route.database_alias(),
                                "execution_id": execution.id,
                                "aggregate": aggregate,
                            });
                            out.write_line(serde_json::to_string(&aggregate_line)?)?;
                        }

                        for attempt in attempts_by_execution
                            .get(&(execution.run_shard, execution.id))
                            .into_iter()
                            .flatten()
                        {
                            let attempt_line = json!({
                                "type": "execution_attempt",
                                "run_id": run_id,
                                "run_shard": route.run_shard(),
                                "database_alias": route.database_alias(),
                                "execution_id": execution.id,
                                "attempt": attempt,
                            });
                            out.write_line(serde_json::to_string(&attempt_line)?)?;

                            for result in results_by_attempt
                                .get(&(attempt.run_shard, attempt.id))
                                .into_iter()
                                .flatten()
                            {
                                let result_line = json!({
                                    "type": "evaluator_result",
                                    "run_id": run_id,
                                    "run_shard": route.run_shard(),
                                    "database_alias": route.database_alias(),
                                    "execution_id": execution.id,
                                    "attempt_id": attempt.id,
                                    "evaluator_result": result,
                                });
                                out.write_line(serde_json::to_string(&result_line)?)?;
                            }
                        }
                    }
                }
            }

            out.flush()?;
        }
    }

    Ok(())
}
