//! Run creation persistence workflow helpers.
//!
//! These helpers write immutable dataset content, dataset membership, the run
//! row, and pending run chunks inside the caller's transaction. Dispatch owns
//! chunk-ready event creation so workers cannot process a run before a
//! coordinator marks it running. Bulk paths are chunked to keep statement size
//! and bind counts bounded for large datasets.

use sqlx::{
    Postgres,
    QueryBuilder,
};
use uuid::Uuid;

use crate::models::{
    case_blob::CaseBlobDraft,
    dataset_version_case::DatasetVersionCaseDraft,
    run::RunDraft,
    run_chunk::RunChunkDraft,
};

const CASE_BLOB_INSERT_CHUNK_SIZE: usize = 500;
const DATASET_MEMBERSHIP_INSERT_CHUNK_SIZE: usize = 2_000;
const RUN_CHUNK_INSERT_CHUNK_SIZE: usize = 2_000;

/// Inserts case blob rows, ignoring already-known content hashes.
pub(crate) async fn bulk_insert_case_blobs(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    case_blobs: &[CaseBlobDraft],
) -> anyhow::Result<()> {
    if case_blobs.is_empty() {
        return Ok(());
    }

    for chunk in case_blobs.chunks(CASE_BLOB_INSERT_CHUNK_SIZE) {
        let mut query_builder = QueryBuilder::<Postgres>::new(
            "INSERT INTO case_blobs (case_hash, task_type, input_payload, expected_output, context_payload, tags, metadata) ",
        );

        query_builder.push_values(chunk, |mut b, row| {
            b.push_bind(&row.case_hash)
                .push_bind(&row.task_type)
                .push_bind(&row.input_payload)
                .push_bind(&row.expected_output)
                .push_bind(&row.context_payload)
                .push_bind(&row.tags)
                .push_bind(&row.metadata);
        });

        query_builder.push(" ON CONFLICT (case_hash) DO NOTHING");
        query_builder.build().execute(tx.as_mut()).await?;
    }

    Ok(())
}

/// Creates or verifies a dataset version identity.
///
/// Existing dataset version ids must refer to the same dataset and version
/// value; otherwise run creation fails to preserve immutable dataset identity.
pub(crate) async fn upsert_dataset_version(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    dataset_version_id: Uuid,
    dataset_id: Uuid,
    dataset_version: &str,
) -> anyhow::Result<()> {
    let rows_affected = sqlx::query(
        r#"
        INSERT INTO dataset_versions (dataset_version_id, dataset_id, dataset_version)
        VALUES ($1, $2, $3)
        ON CONFLICT (dataset_version_id) DO UPDATE
        SET dataset_id = EXCLUDED.dataset_id,
            dataset_version = EXCLUDED.dataset_version,
            updated_at = now()
        WHERE dataset_versions.dataset_id = EXCLUDED.dataset_id
          AND dataset_versions.dataset_version = EXCLUDED.dataset_version
        "#,
    )
    .bind(dataset_version_id)
    .bind(dataset_id)
    .bind(dataset_version)
    .execute(tx.as_mut())
    .await?
    .rows_affected();

    if rows_affected != 1 {
        anyhow::bail!(
            "dataset_version_id '{}' already exists with different dataset identity",
            dataset_version_id
        );
    }

    Ok(())
}

/// Inserts or verifies dataset membership rows for a versioned dataset.
///
/// Existing rows are left untouched to avoid rewriting shared dataset versions
/// for every model run. A follow-up validation query checks that all requested
/// memberships exist with the expected ordinal and case hash.
pub(crate) async fn bulk_insert_dataset_membership(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    dataset_version_id: Uuid,
    cases: &[DatasetVersionCaseDraft],
) -> anyhow::Result<()> {
    if cases.is_empty() {
        return Ok(());
    }

    for chunk in cases.chunks(DATASET_MEMBERSHIP_INSERT_CHUNK_SIZE) {
        let mut query_builder = QueryBuilder::<Postgres>::new(
            "INSERT INTO dataset_version_cases (dataset_version_id, case_id, case_ordinal, case_hash) ",
        );

        query_builder.push_values(chunk, |mut b, row| {
            b.push_bind(dataset_version_id)
                .push_bind(row.case_id)
                .push_bind(row.case_ordinal)
                .push_bind(&row.case_hash);
        });

        query_builder.push(" ON CONFLICT DO NOTHING");
        query_builder.build().execute(tx.as_mut()).await?;

        let mut validation_query = QueryBuilder::<Postgres>::new(
            r#"
            WITH input (case_id, case_ordinal, case_hash) AS (
            "#,
        );
        validation_query.push_values(chunk, |mut b, row| {
            b.push_bind(row.case_id)
                .push_bind(row.case_ordinal)
                .push_bind(&row.case_hash);
        });
        validation_query.push(
            r#"
            )
            SELECT input.case_id
            FROM input
            LEFT JOIN dataset_version_cases dvc
              ON dvc.dataset_version_id =
            "#,
        );
        validation_query.push_bind(dataset_version_id);
        validation_query.push(
            r#"
             AND dvc.case_id = input.case_id
            WHERE dvc.case_id IS NULL
               OR dvc.case_ordinal <> input.case_ordinal
               OR dvc.case_hash <> input.case_hash
            LIMIT 1
            "#,
        );

        let mismatch = validation_query
            .build_query_scalar::<Uuid>()
            .fetch_optional(tx.as_mut())
            .await?;
        if let Some(case_id) = mismatch {
            anyhow::bail!(
                "dataset_version_id '{}' already exists with different membership near case '{}'; dataset versions are immutable",
                dataset_version_id,
                case_id
            );
        }
    }

    Ok(())
}

/// Inserts the run row using the caller-provided id.
pub(crate) async fn insert_run_create(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    draft: &RunDraft,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO runs (
            id,
            run_key,
            dataset_id,
            dataset_version,
            evaluation_profile_id,
            evaluation_profile_version,
            aggregation_policy_id,
            aggregation_policy_version,
            agent_provider,
            agent_name,
            agent_version,
            prompt_config_id,
            prompt_config_version,
            config_snapshot,
            expected_execution_count,
            dataset_version_id,
            profile_version_id,
            profile_hash,
            aggregation_policy_hash
        )
        VALUES (
            $1::uuid,
            $2,
            $3,
            $4,
            $5,
            $6,
            $7,
            $8,
            $9,
            $10,
            $11,
            $12,
            $13,
            $14::jsonb,
            $15,
            $16,
            $17,
            $18,
            $19
        )
        "#,
    )
    .bind(run_id)
    .bind(&draft.run_key)
    .bind(draft.dataset_id)
    .bind(&draft.dataset_version)
    .bind(&draft.evaluation_profile_id)
    .bind(&draft.evaluation_profile_version)
    .bind(&draft.aggregation_policy_id)
    .bind(&draft.aggregation_policy_version)
    .bind(&draft.agent_provider)
    .bind(&draft.agent_name)
    .bind(&draft.agent_version)
    .bind(&draft.prompt_config_id)
    .bind(&draft.prompt_config_version)
    .bind(&draft.config_snapshot)
    .bind(draft.expected_execution_count)
    .bind(draft.dataset_version_id)
    .bind(&draft.profile_version_id)
    .bind(&draft.profile_hash)
    .bind(&draft.aggregation_policy_hash)
    .execute(tx.as_mut())
    .await?;

    Ok(())
}

/// Inserts pending chunk rows for the run.
///
/// Chunk-ready outbox events are created by dispatch after the run is marked
/// running.
pub(crate) async fn bulk_insert_run_chunks(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    dataset_version_id: Uuid,
    chunks: &[RunChunkDraft],
) -> anyhow::Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }

    for chunk_batch in chunks.chunks(RUN_CHUNK_INSERT_CHUNK_SIZE) {
        let mut query_builder = QueryBuilder::<Postgres>::new(
            "INSERT INTO run_chunks (id, run_id, dataset_version_id, profile_group_id, ordinal_start, ordinal_end, status) ",
        );

        query_builder.push_values(chunk_batch, |mut b, chunk| {
            b.push_bind(chunk.chunk_id)
                .push_bind(run_id)
                .push_bind(dataset_version_id)
                .push_bind(&chunk.profile_group_id)
                .push_bind(chunk.ordinal_start)
                .push_bind(chunk.ordinal_end)
                .push_bind("pending");
        });

        query_builder.build().execute(tx.as_mut()).await?;
    }

    Ok(())
}
