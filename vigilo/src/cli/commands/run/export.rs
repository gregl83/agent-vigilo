//! Run export command implementation.
//!
//! Exports run summary data and execution artifacts as either one JSON document
//! or streamed JSONL records. Use JSON for small runs and JSONL for large runs;
//! pagination must stay ordered by `(run_shard, execution_id)` so exports are
//! deterministic and resumable internally.

use super::*;

#[derive(Debug)]
struct RunExportBatch {
    executions: Vec<Execution>,
    attempts: Vec<ExecutionAttempt>,
    aggregates: Vec<ExecutionAggregate>,
    evaluator_results: Vec<EvaluatorResult>,
}

async fn select_execution_batch_by_run_id(
    db: &sqlx::PgPool,
    run_id: Uuid,
    after_execution: Option<(i16, Uuid)>,
    limit: i64,
) -> anyhow::Result<Vec<Execution>> {
    let (after_run_shard, after_execution_id): (Option<i16>, Option<Uuid>) = after_execution
        .map(|(run_shard, execution_id)| (Some(run_shard), Some(execution_id)))
        .unwrap_or((None, None));

    let executions = sqlx::query_as::<_, Execution>(
        r#"
        SELECT
            id,
            run_id,
            run_shard,
            chunk_id,
            case_id,
            task_type,
            tags,
            input_payload,
            expected_output,
            case_metadata,
            evaluation_profile_id,
            evaluation_profile_version,
            evaluator_manifest,
            expected_evaluator_count,
            status::text as status,
            current_attempt_no,
            current_attempt_id,
            last_error_message,
            retry_after,
            retry_count,
            last_attempt_completed_at,
            created_at,
            started_at,
            completed_at,
            updated_at
        FROM executions
        WHERE run_id = $1::uuid
          AND (
              $2::int2 IS NULL
              OR (run_shard, id) > ($2::int2, $3::uuid)
          )
        ORDER BY run_shard, id
        LIMIT $4
        "#,
    )
    .bind(run_id)
    .bind(after_run_shard)
    .bind(after_execution_id)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(executions)
}

async fn select_run_export_batch_for_executions(
    db: &sqlx::PgPool,
    run_id: Uuid,
    executions: Vec<Execution>,
) -> anyhow::Result<RunExportBatch> {
    if executions.is_empty() {
        return Ok(RunExportBatch {
            executions,
            attempts: Vec::new(),
            aggregates: Vec::new(),
            evaluator_results: Vec::new(),
        });
    }

    let execution_ids = executions
        .iter()
        .map(|execution| execution.id)
        .collect::<Vec<_>>();
    let execution_shards = executions
        .iter()
        .map(|execution| execution.run_shard)
        .collect::<Vec<_>>();

    let attempts = sqlx::query_as::<_, ExecutionAttempt>(
        r#"
        WITH input (execution_id, run_shard) AS (
            SELECT *
            FROM UNNEST($2::uuid[], $3::int2[])
        )
        SELECT
            ea.id,
            ea.execution_id,
            ea.run_id,
            ea.run_shard,
            ea.attempt_no,
            ea.status::text as status,
            ea.worker_id,
            ea.worker_host,
            ea.queue_message_id,
            ea.broker_message_id,
            ea.leased_until::text as leased_until,
            ea.heartbeat_at::text as heartbeat_at,
            ea.request_artifact_uri,
            ea.response_artifact_uri,
            ea.agent_latency_ms,
            ea.evaluator_latency_ms,
            ea.total_latency_ms,
            ea.token_usage,
            ea.outcome_summary,
            ea.error_message,
            ea.created_at,
            ea.started_at,
            ea.completed_at,
            ea.updated_at
        FROM execution_attempts ea
        JOIN input
          ON input.execution_id = ea.execution_id
         AND input.run_shard = ea.run_shard
        WHERE ea.run_id = $1::uuid
        ORDER BY ea.execution_id, ea.attempt_no, ea.id
        "#,
    )
    .bind(run_id)
    .bind(&execution_ids)
    .bind(&execution_shards)
    .fetch_all(db)
    .await?;

    let aggregates = sqlx::query_as::<_, ExecutionAggregate>(
        r#"
        WITH input (execution_id, run_shard) AS (
            SELECT *
            FROM UNNEST($2::uuid[], $3::int2[])
        )
        SELECT
            eag.execution_id,
            eag.run_id,
            eag.run_shard,
            eag.attempt_id,
            eag.overall_status::text as overall_status,
            eag.aggregate_score,
            eag.evaluator_result_count,
            eag.dimension_scores,
            eag.blocking_failures,
            eag.summary,
            eag.created_at,
            eag.updated_at
        FROM execution_aggregates eag
        JOIN input
          ON input.execution_id = eag.execution_id
         AND input.run_shard = eag.run_shard
        WHERE eag.run_id = $1::uuid
        ORDER BY eag.execution_id
        "#,
    )
    .bind(run_id)
    .bind(&execution_ids)
    .bind(&execution_shards)
    .fetch_all(db)
    .await?;

    let attempt_ids = attempts
        .iter()
        .map(|attempt| attempt.id)
        .collect::<Vec<_>>();
    let attempt_shards = attempts
        .iter()
        .map(|attempt| attempt.run_shard)
        .collect::<Vec<_>>();

    let evaluator_results = if attempt_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, EvaluatorResult>(
            r#"
            WITH input (attempt_id, run_shard) AS (
                SELECT *
                FROM UNNEST($2::uuid[], $3::int2[])
            )
            SELECT
                er.id,
                er.run_id,
                er.run_shard,
                er.execution_id,
                er.attempt_id,
                er.evaluator_id,
                er.finding_index,
                er.evaluator_version,
                er.evaluator_profile_id,
                er.evaluator_profile_version,
                er.evaluator_interface_version,
                er.evaluator_runtime_version,
                er.dimension,
                er.status::text as status,
                er.blocking,
                er.score_kind,
                er.raw_score,
                er.raw_score_min,
                er.raw_score_max,
                er.normalized_score,
                er.weight,
                er.severity::text as severity,
                er.failure_category,
                er.reason,
                er.evidence,
                er.raw_evaluator_output,
                er.created_at
            FROM evaluator_results er
            JOIN input
              ON input.attempt_id = er.attempt_id
             AND input.run_shard = er.run_shard
            WHERE er.run_id = $1::uuid
            ORDER BY er.execution_id, er.attempt_id, er.evaluator_id, er.finding_index, er.created_at, er.id
            "#,
        )
        .bind(run_id)
        .bind(&attempt_ids)
        .bind(&attempt_shards)
        .fetch_all(db)
        .await?
    };

    Ok(RunExportBatch {
        executions,
        attempts,
        aggregates,
        evaluator_results,
    })
}

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
    let db = context.db().await?.control().await?;
    let out = context.out().await?;

    let run = select_existing_run(db, run_id).await?;
    let summary = select_run_results_summary(db, run_id).await?;

    match format {
        RunExportFormat::Json => {
            // --- Materialize JSON export ---
            // JSON export loads the whole run into memory so callers receive
            // one structured document.
            let mut all_executions = Vec::new();
            let mut all_attempts = Vec::new();
            let mut all_aggregates = Vec::new();
            let mut all_evaluator_results = Vec::new();
            let mut cursor: Option<(i16, Uuid)> = None;

            loop {
                let execution_batch =
                    select_execution_batch_by_run_id(db, run_id, cursor, batch_size).await?;
                if execution_batch.is_empty() {
                    break;
                }

                cursor = execution_batch
                    .last()
                    .map(|execution| (execution.run_shard, execution.id));
                let batch =
                    select_run_export_batch_for_executions(db, run_id, execution_batch).await?;

                all_executions.extend(batch.executions);
                all_attempts.extend(batch.attempts);
                all_aggregates.extend(batch.aggregates);
                all_evaluator_results.extend(batch.evaluator_results);
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

            let mut cursor: Option<(i16, Uuid)> = None;
            loop {
                // --- Load execution page ---
                // Page executions by id and load child attempts, aggregates,
                // and evaluator results only for that page.
                let execution_batch =
                    select_execution_batch_by_run_id(db, run_id, cursor, batch_size).await?;
                if execution_batch.is_empty() {
                    break;
                }

                cursor = execution_batch
                    .last()
                    .map(|execution| (execution.run_shard, execution.id));
                let batch =
                    select_run_export_batch_for_executions(db, run_id, execution_batch).await?;

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
                // Emit one execution record followed by its aggregate, attempts,
                // and attempt result rows. This preserves a stable local
                // grouping without holding the entire export in memory.
                for execution in &batch.executions {
                    let execution_line = json!({
                        "type": "execution",
                        "run_id": run_id,
                        "execution": execution,
                    });
                    out.write_line(serde_json::to_string(&execution_line)?)?;

                    if let Some(aggregate) =
                        aggregates_by_execution.get(&(execution.run_shard, execution.id))
                    {
                        let aggregate_line = json!({
                            "type": "execution_aggregate",
                            "run_id": run_id,
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
                                "execution_id": execution.id,
                                "attempt_id": attempt.id,
                                "evaluator_result": result,
                            });
                            out.write_line(serde_json::to_string(&result_line)?)?;
                        }
                    }
                }
            }

            out.flush()?;
        }
    }

    Ok(())
}
