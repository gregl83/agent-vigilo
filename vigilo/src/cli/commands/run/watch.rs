use super::*;

async fn select_existing_run_for_watch(db: &sqlx::PgPool, run_id: Uuid) -> anyhow::Result<Run> {
    runs::select_run_by_id(db, run_id).await?.ok_or_else(|| {
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
    let db = context.db().await?;
    let out = context.out().await?;
    let interval = Duration::from_secs(interval_seconds);
    let deadline = timeout_seconds.map(|seconds| Instant::now() + Duration::from_secs(seconds));
    let mut last_snapshot = None;
    let mut run = select_existing_run_for_watch(db, run_id).await?;

    loop {
        let terminal = is_terminal_run_status(&run.status);
        let snapshot = RunWatchSnapshotKey::from(&run);

        if last_snapshot.as_ref() != Some(&snapshot) || terminal {
            out.write_value(&run_watch_payload(&run, terminal))?;
            out.flush()?;
            last_snapshot = Some(snapshot);
        }

        if terminal {
            if let Some(reason) = run_terminal_failure_reason(&run) {
                anyhow::bail!(reason);
            }

            if fail_on_gate && let Some(reason) = run_gate_failure_reason(&run) {
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
                    run.status
                );
            }

            deadline.saturating_duration_since(now).min(interval)
        } else {
            interval
        };

        sleep(sleep_for).await;
        run = select_existing_run_for_watch(db, run_id).await?;
    }
}
