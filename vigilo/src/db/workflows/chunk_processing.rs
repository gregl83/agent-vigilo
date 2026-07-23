//! Chunk leasing and case loading workflow helpers.
//!
//! Workers use this module to claim a run chunk, load the corresponding dataset
//! case rows, and then either complete or release the chunk. Lease fields are
//! checked on completion/release so stale workers cannot overwrite a newer
//! lease holder.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    context::database::{
        self,
        ExecutionRoute,
    },
    models::run_chunk::RunChunk,
};

/// Claims a chunk through a previously resolved execution route.
pub(crate) async fn claim_routed_chunk_for_processing(
    database_router: &database::DatabaseRouter,
    route: &ExecutionRoute,
    run_id: Uuid,
    run_shard: i16,
    chunk_id: Uuid,
    lease_seconds: i32,
) -> anyhow::Result<Option<RunChunk>> {
    let mut tx = database_router.begin_execution_admission(route).await?;
    let chunk = claim_chunk_for_processing_in_transaction(
        &mut tx,
        run_id,
        run_shard,
        chunk_id,
        lease_seconds,
    )
    .await?;
    tx.commit().await?;
    Ok(chunk)
}

/// Dataset case row materialized for worker-side execution.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, sqlx::FromRow)]
pub(crate) struct WorkerCaseBatchItem {
    pub(crate) case_id: Uuid,
    pub(crate) case_hash: String,
    pub(crate) case_ordinal: i32,
    pub(crate) task_type: String,
    pub(crate) case_group: Option<String>,
    pub(crate) input_payload: serde_json::Value,
    pub(crate) expected_output: serde_json::Value,
    pub(crate) context_payload: serde_json::Value,
    pub(crate) tags: serde_json::Value,
    pub(crate) metadata: serde_json::Value,
}

/// Claims a pending or expired chunk for a prepared run shard.
///
/// Returns `None` when another worker already owns the current lease or the
/// chunk's run is not currently processable.
///
/// Query behavior: one guarded `UPDATE ... RETURNING` claims ownership only if
/// the chunk is pending or its previous lease has expired and the execution
/// placement has a local run snapshot. Each claim receives an opaque token that
/// remains stable while heartbeats extend its deadline.
async fn claim_chunk_for_processing_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
    run_shard: i16,
    chunk_id: Uuid,
    lease_seconds: i32,
) -> anyhow::Result<Option<RunChunk>> {
    let chunk = sqlx::query_as::<_, RunChunk>(
        r#"
		UPDATE run_chunks
		SET status = 'leased',
			lease_token = gen_random_uuid(),
			leased_until = now() + ($4::int * interval '1 second'),
			updated_at = now()
		WHERE run_id = $1::uuid
          AND run_shard = $2
		  AND id = $3::uuid
		  AND (
			status = 'pending'
			OR (status = 'leased' AND leased_until < now())
		  )
		  AND EXISTS (
			SELECT 1
			FROM run_snapshots rs
			WHERE rs.run_id = run_chunks.run_id
			  AND rs.run_shard = run_chunks.run_shard
		  )
		RETURNING
			id,
			run_id,
            run_shard,
			dataset_version_id,
			profile_group_id,
			ordinal_start,
			ordinal_end,
			status,
			lease_token,
			leased_until,
			created_at,
			updated_at
		"#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(chunk_id)
    .bind(lease_seconds)
    .fetch_optional(&mut **tx)
    .await?;

    Ok(chunk)
}

/// Extends a currently owned chunk lease and returns the updated lease token.
///
/// Lease ownership is represented by the opaque claim token. Heartbeats retain
/// that token while advancing the deadline.
///
/// Query behavior: updates an unexpired lease only for its current token and
/// while the execution placement still has a local run snapshot. `None` means
/// the worker lost authority and should acknowledge the stale message.
pub(crate) async fn extend_chunk_lease(
    db: &PgPool,
    chunk: &RunChunk,
    lease_seconds: i32,
) -> anyhow::Result<Option<RunChunk>> {
    let lease_token = require_chunk_lease_token(chunk)?;
    let chunk = sqlx::query_as::<_, RunChunk>(
        r#"
		UPDATE run_chunks
		SET leased_until = now() + ($4::int * interval '1 second'),
			updated_at = now()
		WHERE run_id = $1::uuid
          AND run_shard = $2
		  AND id = $3::uuid
		  AND status = 'leased'
		  AND lease_token = $5::uuid
		  AND leased_until >= now()
		  AND EXISTS (
			SELECT 1
			FROM run_snapshots rs
			WHERE rs.run_id = run_chunks.run_id
			  AND rs.run_shard = run_chunks.run_shard
		  )
		RETURNING
			id,
			run_id,
            run_shard,
			dataset_version_id,
			profile_group_id,
			ordinal_start,
			ordinal_end,
			status,
			lease_token,
			leased_until,
			created_at,
			updated_at
		"#,
    )
    .bind(chunk.run_id)
    .bind(chunk.run_shard)
    .bind(chunk.id)
    .bind(lease_seconds)
    .bind(lease_token)
    .fetch_optional(db)
    .await?;

    Ok(chunk)
}

/// Loads the dataset cases covered by a claimed chunk's ordinal range.
///
/// Query behavior: reads immutable dataset membership rows joined to case
/// blobs, ordered by ordinal, for `[ordinal_start, ordinal_end)`. This function
/// does not claim work; callers must already own the chunk lease.
pub(crate) async fn load_chunk_case_batch(
    db: &PgPool,
    chunk: &RunChunk,
) -> anyhow::Result<Vec<WorkerCaseBatchItem>> {
    let rows = sqlx::query_as::<_, WorkerCaseBatchItem>(
        r#"
		SELECT
			cvc.case_id,
			cvc.case_hash,
			cvc.case_ordinal,
			cb.task_type,
			cb.case_group,
			cb.input_payload,
			cb.expected_output,
			cb.context_payload,
			cb.tags,
			cb.metadata
		FROM dataset_version_cases cvc
		JOIN case_blobs cb ON cvc.case_hash = cb.case_hash
		WHERE cvc.dataset_version_id = $1
		  AND cvc.case_ordinal >= $2
		  AND cvc.case_ordinal < $3
		ORDER BY cvc.case_ordinal
		"#,
    )
    .bind(chunk.dataset_version_id)
    .bind(chunk.ordinal_start)
    .bind(chunk.ordinal_end)
    .fetch_all(db)
    .await?;

    Ok(rows)
}

/// Marks a leased chunk complete if the caller still owns the same lease.
///
/// Query behavior: clears an unexpired token-owned lease and refreshes the
/// shard summary in the same transaction. A zero row count means the worker is
/// stale.
pub(crate) async fn mark_chunk_completed_and_refresh_summary(
    db: &PgPool,
    chunk: &RunChunk,
) -> anyhow::Result<u64> {
    let lease_token = require_chunk_lease_token(chunk)?;
    let mut tx = db.begin().await?;
    let result = sqlx::query(
        r#"
		UPDATE run_chunks
		SET status = 'completed',
			lease_token = NULL,
			leased_until = NULL,
			updated_at = now()
		WHERE run_id = $1::uuid
          AND run_shard = $2
		  AND id = $3::uuid
		  AND status = 'leased'
		  AND lease_token = $4::uuid
		  AND leased_until >= now()
		"#,
    )
    .bind(chunk.run_id)
    .bind(chunk.run_shard)
    .bind(chunk.id)
    .bind(lease_token)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() > 0 {
        super::run_shard_summary::refresh_run_shard_summary_with(
            &mut *tx,
            chunk.run_id,
            chunk.run_shard,
        )
        .await?;
    }
    tx.commit().await?;

    Ok(result.rows_affected())
}

/// Releases a leased chunk back to pending if the caller still owns the lease.
///
/// Query behavior: clears the lease and returns the chunk to `pending` only for
/// the current lease token. Workers use this for recoverable processing
/// failures and planned execution retry waits.
pub(crate) async fn release_chunk_as_pending(db: &PgPool, chunk: &RunChunk) -> anyhow::Result<u64> {
    let lease_token = require_chunk_lease_token(chunk)?;
    let result = sqlx::query(
        r#"
		UPDATE run_chunks
		SET status = 'pending',
			lease_token = NULL,
			leased_until = NULL,
			updated_at = now()
		WHERE run_id = $1::uuid
          AND run_shard = $2
		  AND id = $3::uuid
		  AND status = 'leased'
		  AND lease_token = $4::uuid
		  AND leased_until >= now()
		"#,
    )
    .bind(chunk.run_id)
    .bind(chunk.run_shard)
    .bind(chunk.id)
    .bind(lease_token)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

/// Marks a leased chunk failed if the caller still owns the lease.
///
/// Query behavior: clears the lease, makes the chunk terminal `failed`, and
/// refreshes the shard summary in one transaction. Workers use this when
/// bounded message retries are exhausted.
pub(crate) async fn mark_chunk_failed_and_refresh_summary(
    db: &PgPool,
    chunk: &RunChunk,
) -> anyhow::Result<u64> {
    let lease_token = require_chunk_lease_token(chunk)?;
    let mut tx = db.begin().await?;
    let result = sqlx::query(
        r#"
		UPDATE run_chunks
		SET status = 'failed',
			lease_token = NULL,
			leased_until = NULL,
			updated_at = now()
		WHERE run_id = $1::uuid
          AND run_shard = $2
		  AND id = $3::uuid
		  AND status = 'leased'
		  AND lease_token = $4::uuid
		  AND leased_until >= now()
		"#,
    )
    .bind(chunk.run_id)
    .bind(chunk.run_shard)
    .bind(chunk.id)
    .bind(lease_token)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() > 0 {
        super::run_shard_summary::refresh_run_shard_summary_with(
            &mut *tx,
            chunk.run_id,
            chunk.run_shard,
        )
        .await?;
    }
    tx.commit().await?;

    Ok(result.rows_affected())
}

fn require_chunk_lease_token(chunk: &RunChunk) -> anyhow::Result<Uuid> {
    chunk.lease_token.ok_or_else(|| {
        anyhow::anyhow!(
            "leased chunk {} for run {} shard {} has no lease token",
            chunk.id,
            chunk.run_id,
            chunk.run_shard
        )
    })
}
