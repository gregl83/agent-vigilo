//! Run watch command implementation.
//!
//! Polls an existing run until it reaches a terminal status, emitting snapshots
//! only when visible state changes or completion occurs. Watch mode never waits
//! for a missing run to appear; callers must create the run first and pass a
//! valid UUID.

use super::*;

async fn select_existing_run_for_watch(
    database_router: &crate::context::database::DatabaseRouter,
    run_id: Uuid,
) -> anyhow::Result<run_status_workflow::RunStatusProjection> {
    run_status_workflow::select_run_status(database_router, run_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "run '{}' was not found; watch only waits for runs that already exist",
                run_id
            )
        })
}

pub(super) async fn exec(
    context: Context,
    run_id: String,
    interval_seconds: u64,
    timeout_seconds: Option<u64>,
    fail_on_gate: bool,
) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let database_router = context.dbr().await?;
    let out = context.out().await?;
    let interval = Duration::from_secs(interval_seconds);
    let deadline = timeout_seconds.map(|seconds| Instant::now() + Duration::from_secs(seconds));
    let mut last_snapshot = None;
    let mut status = select_existing_run_for_watch(database_router, run_id).await?;

    loop {
        let terminal = is_terminal_run_status(&status.run.status);
        let snapshot = RunWatchSnapshotKey::from(&status);

        if last_snapshot.as_ref() != Some(&snapshot) || terminal {
            out.write_value(&run_watch_payload_from_status(&status, terminal))?;
            out.flush()?;
            last_snapshot = Some(snapshot);
        }

        if terminal {
            if let Some(reason) = run_terminal_failure_reason(&status.run) {
                anyhow::bail!(reason);
            }

            if fail_on_gate && let Some(reason) = run_gate_failure_reason(&status.run) {
                anyhow::bail!(reason);
            }
            return Ok(());
        }

        let sleep_for = if let Some(deadline) = deadline {
            let now = Instant::now();
            if now >= deadline {
                anyhow::bail!(
                    "timed out watching run '{}' before terminal status; last status={}",
                    run_id,
                    status.run.status
                );
            }

            deadline.saturating_duration_since(now).min(interval)
        } else {
            interval
        };

        sleep(sleep_for).await;
        status = select_existing_run_for_watch(database_router, run_id).await?;
    }
}
