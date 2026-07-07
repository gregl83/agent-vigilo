//! Run status command implementation.
//!
//! Returns a single run progress snapshot using the same payload shape as
//! watch updates. This command should avoid side effects and should validate
//! the run id before initializing database-backed command work.

use super::*;

pub(super) async fn exec(context: Context, run_id: String) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let db = context.db().await?;
    let out = context.out().await?;
    let run = select_existing_run(db, run_id).await?;
    let payload = run_watch_payload(&run, is_terminal_run_status(&run.status));

    out.write_value(&payload)?;
    Ok(())
}
