use serde_json::json;
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

pub(crate) async fn bulk_insert_case_blobs(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    case_blobs: &[CaseBlobDraft],
) -> anyhow::Result<()> {
    if case_blobs.is_empty() {
        return Ok(());
    }

    let mut query_builder = QueryBuilder::<Postgres>::new(
        "INSERT INTO case_blobs (case_hash, task_type, input_payload, expected_output, context_payload, tags, metadata) ",
    );

    query_builder.push_values(case_blobs, |mut b, row| {
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

    Ok(())
}

pub(crate) async fn upsert_dataset_version(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    dataset_version_id: &str,
    dataset_id: &str,
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

pub(crate) async fn bulk_insert_dataset_membership(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    dataset_version_id: &str,
    cases: &[DatasetVersionCaseDraft],
) -> anyhow::Result<()> {
    if cases.is_empty() {
        return Ok(());
    }

    let mut query_builder = QueryBuilder::<Postgres>::new(
        "INSERT INTO dataset_version_cases (dataset_version_id, case_id, case_ordinal, case_hash) ",
    );

    query_builder.push_values(cases, |mut b, row| {
        b.push_bind(dataset_version_id)
            .push_bind(&row.case_id)
            .push_bind(row.case_ordinal)
            .push_bind(&row.case_hash);
    });

    query_builder.push(
        " ON CONFLICT (dataset_version_id, case_id) DO UPDATE \
         SET case_ordinal = EXCLUDED.case_ordinal, case_hash = EXCLUDED.case_hash \
         WHERE dataset_version_cases.case_ordinal = EXCLUDED.case_ordinal \
           AND dataset_version_cases.case_hash = EXCLUDED.case_hash",
    );

    let rows_affected = query_builder
        .build()
        .execute(tx.as_mut())
        .await?
        .rows_affected();
    if rows_affected != cases.len() as u64 {
        anyhow::bail!(
            "dataset_version_id '{}' already exists with different membership; dataset versions are immutable",
            dataset_version_id
        );
    }

    Ok(())
}

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
            $13::jsonb,
            $14,
            $15,
            $16,
            $17,
            $18
        )
        "#,
    )
    .bind(run_id)
    .bind(&draft.run_key)
    .bind(&draft.dataset_id)
    .bind(&draft.dataset_version)
    .bind(&draft.evaluation_profile_id)
    .bind(&draft.evaluation_profile_version)
    .bind(&draft.aggregation_policy_id)
    .bind(&draft.aggregation_policy_version)
    .bind(&draft.agent_provider)
    .bind(&draft.agent_name)
    .bind(&draft.prompt_config_id)
    .bind(&draft.prompt_config_version)
    .bind(&draft.config_snapshot)
    .bind(draft.expected_execution_count)
    .bind(&draft.dataset_version_id)
    .bind(&draft.profile_version_id)
    .bind(&draft.profile_hash)
    .bind(&draft.aggregation_policy_hash)
    .execute(tx.as_mut())
    .await?;

    Ok(())
}

pub(crate) async fn bulk_insert_run_chunks(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    dataset_version_id: &str,
    chunks: &[RunChunkDraft],
) -> anyhow::Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }

    let mut query_builder = QueryBuilder::<Postgres>::new(
        "INSERT INTO run_chunks (id, run_id, dataset_version_id, profile_group_id, ordinal_start, ordinal_end, status) ",
    );

    query_builder.push_values(chunks, |mut b, chunk| {
        b.push_bind(chunk.chunk_id)
            .push_bind(run_id)
            .push_bind(dataset_version_id)
            .push_bind(&chunk.profile_group_id)
            .push_bind(chunk.ordinal_start)
            .push_bind(chunk.ordinal_end)
            .push_bind("pending");
    });

    query_builder.build().execute(tx.as_mut()).await?;

    Ok(())
}

pub(crate) async fn bulk_enqueue_chunk_events(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    chunks: &[RunChunkDraft],
) -> anyhow::Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }

    let mut query_builder = QueryBuilder::<Postgres>::new(
        "INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload) ",
    );

    query_builder.push_values(chunks, |mut b, chunk| {
        b.push_bind("run.chunk.ready")
            .push_bind("run")
            .push_bind(run_id)
            .push_bind(format!("run:{}:chunk:{}:ready", run_id, chunk.chunk_id))
            .push_bind(json!({
                "run_id": run_id,
                "chunk_id": chunk.chunk_id,
            }));
    });

    query_builder.push(" ON CONFLICT (dedupe_key) DO NOTHING");
    query_builder.build().execute(tx.as_mut()).await?;

    Ok(())
}
