use sqlx::PgPool;
use uuid::Uuid;

use crate::models::run_chunk::RunChunk;

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

pub(crate) async fn claim_chunk_for_processing(
    db: &PgPool,
    chunk_id: Uuid,
    lease_seconds: i32,
) -> anyhow::Result<Option<RunChunk>> {
    let chunk = sqlx::query_as::<_, RunChunk>(
        r#"
		UPDATE run_chunks
		SET status = 'leased',
			leased_until = now() + ($2::int * interval '1 second'),
			updated_at = now()
		WHERE id = $1::uuid
		  AND (
			status = 'pending'
			OR (status = 'leased' AND leased_until < now())
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
    .bind(chunk_id)
    .bind(lease_seconds)
    .fetch_optional(db)
    .await?;

    Ok(chunk)
}

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

pub(crate) async fn mark_chunk_completed(db: &PgPool, chunk_id: Uuid) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
		UPDATE run_chunks
		SET status = 'completed',
			leased_until = NULL,
			updated_at = now()
		WHERE id = $1::uuid
		"#,
    )
    .bind(chunk_id)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

pub(crate) async fn release_chunk_as_pending(db: &PgPool, chunk_id: Uuid) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
		UPDATE run_chunks
		SET status = 'pending',
			leased_until = NULL,
			updated_at = now()
		WHERE id = $1::uuid
		"#,
    )
    .bind(chunk_id)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}
