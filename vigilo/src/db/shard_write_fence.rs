//! Transaction-scoped admission locks for execution-owned shard writes.
//!
//! Claims, dispatch, and routed cancellation cleanup take the shared lock.
//! Shard movement takes the exclusive lock, changes the control route, and
//! checks for previously admitted work before copying. PostgreSQL releases
//! either lock with the transaction, including rollback and connection loss.

use sqlx::{
    Postgres,
    Transaction,
};
use uuid::Uuid;

pub(crate) async fn lock_shared(
    tx: &mut Transaction<'_, Postgres>,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        SELECT pg_advisory_xact_lock_shared(
            hashtextextended($1::uuid::text, $2::bigint)
        )
        "#,
    )
    .bind(run_id)
    .bind(i64::from(run_shard))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn lock_exclusive(
    tx: &mut Transaction<'_, Postgres>,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        SELECT pg_advisory_xact_lock(
            hashtextextended($1::uuid::text, $2::bigint)
        )
        "#,
    )
    .bind(run_id)
    .bind(i64::from(run_shard))
    .execute(&mut **tx)
    .await?;
    Ok(())
}
