//! Run creation persistence workflow helpers.
//!
//! These helpers write immutable dataset content, dataset membership, the run
//! row, pending run chunks, and shard placement rows inside the caller's
//! transaction. Dispatch owns chunk-ready event creation in bounded windows so
//! workers cannot process a run before a coordinator marks it running. Bulk
//! paths are chunked to keep statement size and bind counts bounded for large
//! datasets.

use std::collections::BTreeSet;

use sqlx::{
    Postgres,
    QueryBuilder,
};
use uuid::Uuid;

use crate::models::{
    case_blob::CaseBlobDraft,
    database_placement::{
        DATABASE_PLACEMENT_ROLE_CONTROL_AND_SHARD,
        DATABASE_PLACEMENT_ROLE_SHARD,
    },
    dataset_version_case::DatasetVersionCaseDraft,
    run::RunDraft,
    run_chunk::RunChunkDraft,
    shard_placement::SHARD_PLACEMENT_STATUS_ACTIVE,
};

const CASE_BLOB_INSERT_CHUNK_SIZE: usize = 500;
const DATASET_MEMBERSHIP_INSERT_CHUNK_SIZE: usize = 2_000;
const RUN_CHUNK_INSERT_CHUNK_SIZE: usize = 2_000;

/// Inserts case blob rows, ignoring already-known content hashes.
///
/// Query behavior: bulk inserts immutable content-addressed case payloads in
/// bounded batches. `ON CONFLICT (case_hash) DO NOTHING` makes shared case
/// blobs reusable across runs and dataset versions.
pub(crate) async fn bulk_insert_case_blobs(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    case_blobs: &[CaseBlobDraft],
) -> anyhow::Result<()> {
    if case_blobs.is_empty() {
        return Ok(());
    }

    for chunk in case_blobs.chunks(CASE_BLOB_INSERT_CHUNK_SIZE) {
        let mut query_builder = QueryBuilder::<Postgres>::new(
            "INSERT INTO case_blobs (case_hash, task_type, case_group, input_payload, expected_output, context_payload, tags, metadata) ",
        );

        query_builder.push_values(chunk, |mut b, row| {
            b.push_bind(&row.case_hash)
                .push_bind(&row.task_type)
                .push_bind(&row.case_group)
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
///
/// Query behavior: upserts by `dataset_version_id` but only allows the conflict
/// update when the existing row has the same dataset identity. A zero affected
/// row count means the id already belongs to different dataset content.
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
///
/// Query behavior:
/// - Bulk inserts membership rows in bounded batches with conflict no-op.
/// - For each batch, builds an inline input table and left joins persisted rows
///   to detect missing or mismatched membership.
/// - Fails if the dataset version id already describes different case
///   ordering/content.
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
///
/// Query behavior: writes the run metadata and immutable config/profile
/// snapshots in `pending` state. No worker-visible chunk events are emitted
/// here; dispatch owns making the run visible after validation and creation
/// commit.
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
/// Chunk-ready outbox events are created by dispatch windows after the run is
/// marked running.
///
/// Query behavior: bulk inserts run-local chunk ranges in bounded batches. Each
/// chunk points at the immutable dataset version and starts as `pending` with
/// no worker lease.
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
            "INSERT INTO run_chunks (id, run_id, run_shard, dataset_version_id, profile_group_id, ordinal_start, ordinal_end, status) ",
        );

        query_builder.push_values(chunk_batch, |mut b, chunk| {
            b.push_bind(chunk.chunk_id)
                .push_bind(run_id)
                .push_bind(chunk.run_shard)
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

/// Inserts one dispatch cursor per run shard used by the run's chunk set.
///
/// Dispatch cursors let coordinators claim a `(run_id, run_shard)` pair before
/// selecting chunks, keeping dispatch scans aligned with the `run_chunks`
/// partition key.
pub(crate) async fn bulk_insert_run_shard_dispatch_cursors(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    chunks: &[RunChunkDraft],
) -> anyhow::Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }

    let run_shards = chunks
        .iter()
        .map(|chunk| chunk.run_shard)
        .collect::<BTreeSet<_>>();

    let mut query_builder = QueryBuilder::<Postgres>::new(
        "INSERT INTO run_shard_dispatch_cursors (run_id, run_shard, status) ",
    );

    query_builder.push_values(run_shards, |mut b, run_shard| {
        b.push_bind(run_id).push_bind(run_shard).push_bind("open");
    });

    query_builder.push(" ON CONFLICT (run_id, run_shard) DO NOTHING");
    query_builder.build().execute(tx.as_mut()).await?;

    Ok(())
}

/// Inserts one active execution placement row per run shard used by the run.
///
/// The chunk planner already assigned `run_shard` values. This workflow stores
/// the durable routing decision for each distinct `run_id + run_shard` before
/// the run can be dispatched.
pub(crate) async fn bulk_insert_shard_placements(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    chunks: &[RunChunkDraft],
    database_alias: &str,
) -> anyhow::Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }

    validate_active_shard_capable_placement(tx, database_alias).await?;

    let run_shards = chunks
        .iter()
        .map(|chunk| chunk.run_shard)
        .collect::<BTreeSet<_>>();

    let mut query_builder = QueryBuilder::<Postgres>::new(
        "INSERT INTO shard_placements (run_id, run_shard, database_alias, status) ",
    );

    query_builder.push_values(run_shards, |mut b, run_shard| {
        b.push_bind(run_id)
            .push_bind(run_shard)
            .push_bind(database_alias)
            .push_bind(SHARD_PLACEMENT_STATUS_ACTIVE);
    });

    query_builder.build().execute(tx.as_mut()).await?;

    Ok(())
}

async fn validate_active_shard_capable_placement(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    database_alias: &str,
) -> anyhow::Result<()> {
    let placement = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT role, status
        FROM database_placements
        WHERE alias = $1
        "#,
    )
    .bind(database_alias)
    .fetch_optional(tx.as_mut())
    .await?;

    let Some((role, status)) = placement else {
        anyhow::bail!(
            "database placement alias {} is not configured",
            database_alias
        );
    };

    if status != "active" {
        anyhow::bail!(
            "database placement alias {} has status {}, which cannot receive new shard placements",
            database_alias,
            status
        );
    }

    if !matches!(
        role.as_str(),
        DATABASE_PLACEMENT_ROLE_SHARD | DATABASE_PLACEMENT_ROLE_CONTROL_AND_SHARD
    ) {
        anyhow::bail!(
            "database placement alias {} has role {}, which is not shard-capable",
            database_alias,
            role
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    async fn insert_minimal_run(pool: &PgPool, run_id: Uuid) {
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();

        sqlx::query(
            r#"
            INSERT INTO dataset_versions (dataset_version_id, dataset_id, dataset_version)
            VALUES ($1::uuid, $2::uuid, 'test')
            "#,
        )
        .bind(dataset_version_id)
        .bind(dataset_id)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO runs (
                id,
                run_key,
                dataset_id,
                dataset_version_id,
                dataset_version,
                evaluation_profile_id,
                evaluation_profile_version,
                profile_version_id,
                profile_hash,
                aggregation_policy_id,
                aggregation_policy_version,
                aggregation_policy_hash,
                agent_provider,
                agent_name,
                prompt_config_id,
                prompt_config_version,
                expected_execution_count
            )
            VALUES (
                $1::uuid,
                $2,
                $3::uuid,
                $4::uuid,
                'test',
                'profile',
                '1.0.0',
                'profile-version',
                'profile-hash',
                'aggregation',
                '1.0.0',
                'aggregation-hash',
                'example',
                'agent',
                'prompt',
                '1.0.0',
                3
            )
            "#,
        )
        .bind(run_id)
        .bind(format!("run-{run_id}"))
        .bind(dataset_id)
        .bind(dataset_version_id)
        .execute(pool)
        .await
        .unwrap();
    }

    fn chunk(run_shard: i16, ordinal: i32) -> RunChunkDraft {
        RunChunkDraft {
            chunk_id: Uuid::now_v7(),
            run_shard,
            profile_group_id: "default".to_string(),
            ordinal_start: ordinal,
            ordinal_end: ordinal + 1,
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn bulk_insert_run_shard_dispatch_cursors_inserts_distinct_open_shards(pool: PgPool) {
        let run_id = Uuid::now_v7();
        insert_minimal_run(&pool, run_id).await;

        let chunks = vec![chunk(0, 0), chunk(1, 1), chunk(1, 2)];
        let mut tx = pool.begin().await.unwrap();
        bulk_insert_run_shard_dispatch_cursors(&mut tx, run_id, &chunks)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let rows = sqlx::query_as::<_, (i16, String)>(
            r#"
            SELECT run_shard, status
            FROM run_shard_dispatch_cursors
            WHERE run_id = $1::uuid
            ORDER BY run_shard
            "#,
        )
        .bind(run_id)
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(rows, vec![(0, "open".to_string()), (1, "open".to_string())]);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn bulk_insert_shard_placements_inserts_distinct_active_shards(pool: PgPool) {
        let run_id = Uuid::now_v7();
        insert_minimal_run(&pool, run_id).await;

        let chunks = vec![chunk(0, 0), chunk(1, 1), chunk(1, 2)];
        let mut tx = pool.begin().await.unwrap();
        bulk_insert_shard_placements(&mut tx, run_id, &chunks, "primary")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let rows = sqlx::query_as::<_, (i16, String, String)>(
            r#"
            SELECT run_shard, database_alias, status
            FROM shard_placements
            WHERE run_id = $1::uuid
            ORDER BY run_shard
            "#,
        )
        .bind(run_id)
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(
            rows,
            vec![
                (0, "primary".to_string(), "active".to_string()),
                (1, "primary".to_string(), "active".to_string())
            ]
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn bulk_insert_shard_placements_rejects_control_only_alias(pool: PgPool) {
        let run_id = Uuid::now_v7();
        insert_minimal_run(&pool, run_id).await;

        sqlx::query(
            r#"
            UPDATE database_placements
            SET role = 'control'
            WHERE alias = 'primary'
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        let chunks = vec![chunk(0, 0)];
        let mut tx = pool.begin().await.unwrap();
        let error = bulk_insert_shard_placements(&mut tx, run_id, &chunks, "primary")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("not shard-capable"));
    }
}
