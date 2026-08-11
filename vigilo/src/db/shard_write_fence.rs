//! Transaction-scoped admission locks for execution-owned shard writes.
//!
//! Runtime claims, settlement, and admitted mover writes take the shared lock.
//! Shard movement takes the exclusive lock for source lifecycle transitions
//! and target authority installation. Target writes validate the installed
//! move generation and token while holding the shared lock. PostgreSQL releases
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
