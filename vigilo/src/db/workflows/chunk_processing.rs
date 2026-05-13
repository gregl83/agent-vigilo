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
    pub(crate) case_id: String,
    pub(crate) case_hash: String,
    pub(crate) case_ordinal: i32,
    pub(crate) task_type: String,
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
pub(crate) async fn claim_chunk_for_processing(
    db: &PgPool,
    run_id: Uuid,
    chunk_id: Uuid,
    lease_seconds: i32,
) -> anyhow::Result<Option<RunChunk>> {
    let chunk = sqlx::query_as::<_, RunChunk>(
        r#"
		UPDATE run_chunks
		SET status = 'leased',
			leased_until = now() + ($3::int * interval '1 second'),
			updated_at = now()
		WHERE run_id = $1::uuid
		  AND id = $2::uuid
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
    .bind(chunk_id)
    .bind(lease_seconds)
    .fetch_optional(db)
    .await?;

    Ok(chunk)
}

/// Loads the dataset cases covered by a claimed chunk's ordinal range.
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
    .bind(&chunk.dataset_version_id)
    .bind(chunk.ordinal_start)
    .bind(chunk.ordinal_end)
    .fetch_all(db)
    .await?;

    Ok(rows)
}

/// Marks a leased chunk complete if the caller still owns the same lease.
pub(crate) async fn mark_chunk_completed(db: &PgPool, chunk: &RunChunk) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
		UPDATE run_chunks
		SET status = 'completed',
			leased_until = NULL,
			updated_at = now()
		WHERE run_id = $1::uuid
		  AND id = $2::uuid
		  AND status = 'leased'
		  AND leased_until IS NOT DISTINCT FROM $3::timestamptz
		"#,
    )
    .bind(chunk.run_id)
    .bind(chunk.id)
    .bind(chunk.leased_until)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

/// Releases a leased chunk back to pending if the caller still owns the lease.
pub(crate) async fn release_chunk_as_pending(db: &PgPool, chunk: &RunChunk) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
		UPDATE run_chunks
		SET status = 'pending',
			leased_until = NULL,
			updated_at = now()
		WHERE run_id = $1::uuid
		  AND id = $2::uuid
		  AND status = 'leased'
		  AND leased_until IS NOT DISTINCT FROM $3::timestamptz
		"#,
    )
    .bind(chunk.run_id)
    .bind(chunk.id)
    .bind(chunk.leased_until)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}
