//! Chunk leasing and case loading workflow helpers.
//!
//! Workers use this module to claim a run chunk, load the corresponding dataset
//! case rows, and then either complete or release the chunk. Lease fields are
//! checked on completion/release so stale workers cannot overwrite a newer
//! lease holder.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::run_chunk::RunChunk;

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

/// Claims a pending or expired chunk for a running run.
///
/// Returns `None` when another worker already owns the current lease or the
/// chunk's run is not currently processable.
///
/// Query behavior: one guarded `UPDATE ... RETURNING` claims ownership only if
/// the chunk is pending or its previous lease has expired and the parent run is
/// still `running`. The returned `leased_until` value is the worker's lease
/// token for later completion, release, or failure.
pub(crate) async fn claim_chunk_for_processing(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    chunk_id: Uuid,
    lease_seconds: i32,
) -> anyhow::Result<Option<RunChunk>> {
    let chunk = sqlx::query_as::<_, RunChunk>(
        r#"
		UPDATE run_chunks
		SET status = 'leased',
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
			FROM runs
			WHERE runs.id = run_chunks.run_id
			  AND runs.status = 'running'::run_status
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
			leased_until,
			created_at,
			updated_at
		"#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(chunk_id)
    .bind(lease_seconds)
    .fetch_optional(db)
    .await?;

    Ok(chunk)
}

/// Extends a currently owned chunk lease and returns the updated lease token.
///
/// Lease ownership is represented by the `leased_until` value returned at
/// claim/extension time. Completion and release must use the latest returned
/// row, otherwise stale workers cannot overwrite the current owner.
///
/// Query behavior: updates the lease only when the chunk is still leased by the
/// same timestamp token and the parent run is still `running`. `None` means the
/// worker lost authority and should acknowledge the stale message.
pub(crate) async fn extend_chunk_lease(
    db: &PgPool,
    chunk: &RunChunk,
    lease_seconds: i32,
) -> anyhow::Result<Option<RunChunk>> {
    let chunk = sqlx::query_as::<_, RunChunk>(
        r#"
		UPDATE run_chunks
		SET leased_until = now() + ($4::int * interval '1 second'),
			updated_at = now()
		WHERE run_id = $1::uuid
          AND run_shard = $2
		  AND id = $3::uuid
		  AND status = 'leased'
		  AND leased_until IS NOT DISTINCT FROM $5::timestamptz
		  AND EXISTS (
			SELECT 1
			FROM runs
			WHERE runs.id = run_chunks.run_id
			  AND runs.status = 'running'::run_status
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
			leased_until,
			created_at,
			updated_at
		"#,
    )
    .bind(chunk.run_id)
    .bind(chunk.run_shard)
    .bind(chunk.id)
    .bind(lease_seconds)
    .bind(chunk.leased_until)
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
/// Query behavior: clears the lease and moves the chunk to `completed` only if
/// the stored lease timestamp still matches the worker's latest token. A zero
/// row count means the worker is stale.
pub(crate) async fn mark_chunk_completed(db: &PgPool, chunk: &RunChunk) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
		UPDATE run_chunks
		SET status = 'completed',
			leased_until = NULL,
			updated_at = now()
		WHERE run_id = $1::uuid
          AND run_shard = $2
		  AND id = $3::uuid
		  AND status = 'leased'
		  AND leased_until IS NOT DISTINCT FROM $4::timestamptz
		"#,
    )
    .bind(chunk.run_id)
    .bind(chunk.run_shard)
    .bind(chunk.id)
    .bind(chunk.leased_until)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

/// Releases a leased chunk back to pending if the caller still owns the lease.
///
/// Query behavior: clears the lease and returns the chunk to `pending` only for
/// the current lease token. Workers use this for recoverable processing
/// failures and planned execution retry waits.
pub(crate) async fn release_chunk_as_pending(db: &PgPool, chunk: &RunChunk) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
		UPDATE run_chunks
		SET status = 'pending',
			leased_until = NULL,
			updated_at = now()
		WHERE run_id = $1::uuid
          AND run_shard = $2
		  AND id = $3::uuid
		  AND status = 'leased'
		  AND leased_until IS NOT DISTINCT FROM $4::timestamptz
		"#,
    )
    .bind(chunk.run_id)
    .bind(chunk.run_shard)
    .bind(chunk.id)
    .bind(chunk.leased_until)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

/// Marks a leased chunk failed if the caller still owns the lease.
///
/// Query behavior: clears the lease and makes the chunk terminal `failed` only
/// for the current lease token. Workers use this when bounded message retries
/// are exhausted.
pub(crate) async fn mark_chunk_failed(db: &PgPool, chunk: &RunChunk) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
		UPDATE run_chunks
		SET status = 'failed',
			leased_until = NULL,
			updated_at = now()
		WHERE run_id = $1::uuid
          AND run_shard = $2
		  AND id = $3::uuid
		  AND status = 'leased'
		  AND leased_until IS NOT DISTINCT FROM $4::timestamptz
		"#,
    )
    .bind(chunk.run_id)
    .bind(chunk.run_shard)
    .bind(chunk.id)
    .bind(chunk.leased_until)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}
