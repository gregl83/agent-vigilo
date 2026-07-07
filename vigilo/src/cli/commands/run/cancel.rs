//! Run cancellation command implementation.
//!
//! Cancels an active run through the workflow layer and reports the terminal
//! run snapshot plus counts of closed chunks, executions, attempts, and outbox
//! events. Run identifiers must be UUIDs; cancellation behavior must remain
//! idempotent and workflow-owned.

use super::*;

pub(super) fn run_cancel_payload(outcome: &run_cancel::CancelRunOutcome) -> Value {
    json!({
        "data": {
            "run_id": outcome.run.id,
            "run_key": outcome.run.run_key,
            "status": outcome.run.status,
            "gate_status": outcome.run.gate_status,
            "expected_execution_count": outcome.run.expected_execution_count,
            "terminal_execution_count": outcome.run.terminal_execution_count,
            "passed_execution_count": outcome.run.passed_execution_count,
            "failed_execution_count": outcome.run.failed_execution_count,
            "errored_execution_count": outcome.run.errored_execution_count,
            "summary": outcome.run.summary,
            "error_message": outcome.run.error_message,
            "completed_at": outcome.run.completed_at,
            "updated_at": outcome.run.updated_at,
        },
        "meta": {
            "cancelled": outcome.cancelled,
            "already_cancelled": outcome.already_cancelled,
            "terminal": true,
            "chunks_cancelled": outcome.chunks_cancelled,
            "executions_cancelled": outcome.executions_cancelled,
            "attempts_cancelled": outcome.attempts_cancelled,
            "outbox_events_enqueued": outcome.outbox_events_enqueued,
        }
    })
}

pub(super) async fn exec(context: Context, run_id: String) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let db = context.db().await?;
    let out = context.out().await?;
    let outcome = run_cancel::cancel_run(db, run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run '{}' was not found", run_id))?;

    out.write_value(&run_cancel_payload(&outcome))?;
    Ok(())
}
