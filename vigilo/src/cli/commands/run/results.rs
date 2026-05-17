use super::*;

pub(super) fn run_results_payload(run: &Run, summary: &RunResultsSummary) -> Value {
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
        },
        "meta": {
            "summary_only": true,
        }
    })
}

pub(super) async fn exec(context: Context, run_id: String) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let db = context.db().await?;
    let out = context.out().await?;
    let run = select_existing_run(db, run_id).await?;
    let summary = select_run_results_summary(db, run_id).await?;
    let payload = run_results_payload(&run, &summary);

    out.write_value(&payload)?;
    Ok(())
}
