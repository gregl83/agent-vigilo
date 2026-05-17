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
    after_execution_id: Option<Uuid>,
    limit: i64,
) -> anyhow::Result<Vec<Execution>> {
    let executions = sqlx::query_as::<_, Execution>(
        r#"
        SELECT
            id,
            run_id,
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
            created_at,
            started_at,
            completed_at,
            updated_at
        FROM executions
        WHERE run_id = $1::uuid
          AND ($2::uuid IS NULL OR id > $2::uuid)
        ORDER BY id
        LIMIT $3
        "#,
    )
    .bind(run_id)
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

    let attempts = sqlx::query_as::<_, ExecutionAttempt>(
        r#"
        SELECT
            id,
            execution_id,
            run_id,
            attempt_no,
            status::text as status,
            worker_id,
            worker_host,
            queue_message_id,
            leased_until::text as leased_until,
            heartbeat_at::text as heartbeat_at,
            request_artifact_uri,
            response_artifact_uri,
            agent_latency_ms,
            evaluator_latency_ms,
            total_latency_ms,
            token_usage,
            outcome_summary,
            error_message,
            created_at,
            started_at,
            completed_at,
            updated_at
        FROM execution_attempts
        WHERE run_id = $1::uuid
          AND execution_id = ANY($2::uuid[])
        ORDER BY execution_id, attempt_no, id
        "#,
    )
    .bind(run_id)
    .bind(&execution_ids)
    .fetch_all(db)
    .await?;

    let aggregates = sqlx::query_as::<_, ExecutionAggregate>(
        r#"
        SELECT
            execution_id,
            run_id,
            attempt_id,
            overall_status::text as overall_status,
            aggregate_score,
            evaluator_result_count,
            dimension_scores,
            blocking_failures,
            summary,
            created_at,
            updated_at
        FROM execution_aggregates
        WHERE run_id = $1::uuid
          AND execution_id = ANY($2::uuid[])
        ORDER BY execution_id
        "#,
    )
    .bind(run_id)
    .bind(&execution_ids)
    .fetch_all(db)
    .await?;

    let attempt_ids = attempts
        .iter()
        .map(|attempt| attempt.id)
        .collect::<Vec<_>>();

    let evaluator_results = if attempt_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, EvaluatorResult>(
            r#"
            SELECT
                id,
                run_id,
                execution_id,
                attempt_id,
                evaluator_id,
                evaluator_version,
                evaluator_profile_id,
                evaluator_profile_version,
                evaluator_interface_version,
                evaluator_runtime_version,
                dimension,
                status::text as status,
                blocking,
                score_kind,
                raw_score,
                raw_score_min,
                raw_score_max,
                normalized_score,
                weight,
                severity::text as severity,
                failure_category,
                reason,
                evidence,
                raw_evaluator_output,
                created_at
            FROM evaluator_results
            WHERE run_id = $1::uuid
              AND attempt_id = ANY($2::uuid[])
            ORDER BY execution_id, attempt_id, created_at, id
            "#,
        )
        .bind(run_id)
        .bind(&attempt_ids)
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
    let mut attempts_by_execution: BTreeMap<Uuid, Vec<&ExecutionAttempt>> = BTreeMap::new();
    for attempt in attempts {
        attempts_by_execution
            .entry(attempt.execution_id)
            .or_default()
            .push(attempt);
    }

    let mut aggregates_by_execution: BTreeMap<Uuid, &ExecutionAggregate> = BTreeMap::new();
    for aggregate in aggregates {
        aggregates_by_execution.insert(aggregate.execution_id, aggregate);
    }

    let mut results_by_attempt: BTreeMap<Uuid, Vec<&EvaluatorResult>> = BTreeMap::new();
    for result in evaluator_results {
        results_by_attempt
            .entry(result.attempt_id)
            .or_default()
            .push(result);
    }

    let exported_executions = executions
        .iter()
        .map(|execution| {
            let exported_attempts = attempts_by_execution
                .get(&execution.id)
                .into_iter()
                .flatten()
                .map(|attempt| {
                    let attempt_results = results_by_attempt
                        .get(&attempt.id)
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
                .get(&execution.id)
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
    if batch_size <= 0 {
        anyhow::bail!("export batch_size must be greater than zero");
    }

    let run_id = parse_run_id(&run_id)?;
    let db = context.db().await?;
    let out = context.out().await?;

    let run = select_existing_run(db, run_id).await?;
    let summary = select_run_results_summary(db, run_id).await?;

    match format {
        RunExportFormat::Json => {
            let mut all_executions = Vec::new();
            let mut all_attempts = Vec::new();
            let mut all_aggregates = Vec::new();
            let mut all_evaluator_results = Vec::new();
            let mut cursor = None;

            loop {
                let execution_batch =
                    select_execution_batch_by_run_id(db, run_id, cursor, batch_size).await?;
                if execution_batch.is_empty() {
                    break;
                }

                cursor = execution_batch.last().map(|execution| execution.id);
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

            let mut cursor = None;
            loop {
                let execution_batch =
                    select_execution_batch_by_run_id(db, run_id, cursor, batch_size).await?;
                if execution_batch.is_empty() {
                    break;
                }

                cursor = execution_batch.last().map(|execution| execution.id);
                let batch =
                    select_run_export_batch_for_executions(db, run_id, execution_batch).await?;

                let mut attempts_by_execution: BTreeMap<Uuid, Vec<&ExecutionAttempt>> =
                    BTreeMap::new();
                for attempt in &batch.attempts {
                    attempts_by_execution
                        .entry(attempt.execution_id)
                        .or_default()
                        .push(attempt);
                }

                let mut aggregates_by_execution: BTreeMap<Uuid, &ExecutionAggregate> =
                    BTreeMap::new();
                for aggregate in &batch.aggregates {
                    aggregates_by_execution.insert(aggregate.execution_id, aggregate);
                }

                let mut results_by_attempt: BTreeMap<Uuid, Vec<&EvaluatorResult>> = BTreeMap::new();
                for result in &batch.evaluator_results {
                    results_by_attempt
                        .entry(result.attempt_id)
                        .or_default()
                        .push(result);
                }

                for execution in &batch.executions {
                    let execution_line = json!({
                        "type": "execution",
                        "run_id": run_id,
                        "execution": execution,
                    });
                    out.write_line(serde_json::to_string(&execution_line)?)?;

                    if let Some(aggregate) = aggregates_by_execution.get(&execution.id) {
                        let aggregate_line = json!({
                            "type": "execution_aggregate",
                            "run_id": run_id,
                            "execution_id": execution.id,
                            "aggregate": aggregate,
                        });
                        out.write_line(serde_json::to_string(&aggregate_line)?)?;
                    }

                    for attempt in attempts_by_execution
                        .get(&execution.id)
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

                        for result in results_by_attempt.get(&attempt.id).into_iter().flatten() {
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
