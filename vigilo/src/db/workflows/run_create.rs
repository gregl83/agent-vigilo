//! Run creation persistence workflow helpers.
//!
//! These helpers write immutable dataset content, dataset membership, control
//! run metadata, shard placement rows, dispatch cursors, and execution-local
//! pending chunks. Dispatch owns chunk-ready event creation in bounded windows
//! so workers cannot process a run before a coordinator marks it running. Bulk
//! paths are chunked to keep statement size and bind counts bounded for large
//! datasets.

use std::collections::{
    BTreeMap,
    BTreeSet,
};

use sqlx::{
    Postgres,
    QueryBuilder,
    types::Json,
};
use uuid::Uuid;

use crate::{
    context::database::{
        self,
        ShardAssignmentPolicy,
    },
    models::{
        case_blob::CaseBlobDraft,
        database_placement::{
            DATABASE_PLACEMENT_ROLE_CONTROL_AND_SHARD,
            DATABASE_PLACEMENT_ROLE_SHARD,
        },
        dataset_version_case::DatasetVersionCaseDraft,
        run::RunDraft,
        run_chunk::RunChunkDraft,
        shard_placement::SHARD_PLACEMENT_STATUS_ACTIVE,
    },
};

const CASE_BLOB_INSERT_CHUNK_SIZE: usize = 500;
const DATASET_MEMBERSHIP_INSERT_CHUNK_SIZE: usize = 2_000;
const RUN_CHUNK_INSERT_CHUNK_SIZE: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunShardPlacementAssignment {
    pub(crate) run_shard: i16,
    pub(crate) database_alias: String,
}

struct RunSeedState<'a> {
    run_id: Uuid,
    draft: &'a RunDraft,
    case_blobs: &'a [CaseBlobDraft],
    dataset_cases: &'a [DatasetVersionCaseDraft],
    chunks: &'a [RunChunkDraft],
}

/// Persists a newly planned run across control and execution storage.
///
/// Control storage owns the authoritative run metadata, canonical dataset
/// rows, placement rows, and dispatch cursors. Execution storage owns the
/// pending chunks and local copies of the FK/case rows workers need to process
/// those chunks.
pub(crate) async fn insert_run_seed_state(
    database: &database::Db,
    run_id: Uuid,
    draft: &RunDraft,
    case_blobs: &[CaseBlobDraft],
    dataset_cases: &[DatasetVersionCaseDraft],
    chunks: &[RunChunkDraft],
    assignments: &[RunShardPlacementAssignment],
) -> anyhow::Result<()> {
    let seed = RunSeedState {
        run_id,
        draft,
        case_blobs,
        dataset_cases,
        chunks,
    };
    let control_db = database.control().await?;
    let chunks_by_alias = group_chunks_by_assigned_alias(chunks, assignments)?;

    let mut control_tx = control_db.begin().await?;
    let control_chunks = chunks_by_alias
        .get(database.control_database_alias())
        .map(Vec::as_slice)
        .unwrap_or_default();
    insert_control_seed_state(&mut control_tx, &seed, assignments, control_chunks).await?;

    for (database_alias, execution_chunks) in chunks_by_alias {
        if database_alias == database.control_database_alias() {
            continue;
        }
        let execution_db = database.placement(&database_alias).await?;
        let mut execution_tx = execution_db.begin().await?;
        insert_execution_seed_state(&mut execution_tx, &seed, &execution_chunks).await?;
        execution_tx.commit().await?;
    }

    control_tx.commit().await?;
    Ok(())
}

async fn insert_control_seed_state(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    seed: &RunSeedState<'_>,
    assignments: &[RunShardPlacementAssignment],
    control_chunks: &[RunChunkDraft],
) -> anyhow::Result<()> {
    bulk_insert_case_blobs(tx, seed.case_blobs).await?;
    upsert_dataset_version(
        tx,
        seed.draft.dataset_version_id,
        seed.draft.dataset_id,
        &seed.draft.dataset_version,
    )
    .await?;
    bulk_insert_dataset_membership(tx, seed.draft.dataset_version_id, seed.dataset_cases).await?;
    insert_run_create(tx, seed.run_id, seed.draft).await?;
    bulk_insert_shard_placements(tx, seed.run_id, assignments).await?;
    bulk_insert_run_shard_dispatch_cursors(tx, seed.run_id, seed.chunks).await?;

    if !control_chunks.is_empty() {
        bulk_insert_run_chunks(
            tx,
            seed.run_id,
            seed.draft.dataset_version_id,
            control_chunks,
        )
        .await?;
    }

    Ok(())
}

async fn insert_execution_seed_state(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    seed: &RunSeedState<'_>,
    chunks: &[RunChunkDraft],
) -> anyhow::Result<()> {
    bulk_insert_case_blobs(tx, seed.case_blobs).await?;
    upsert_dataset_version(
        tx,
        seed.draft.dataset_version_id,
        seed.draft.dataset_id,
        &seed.draft.dataset_version,
    )
    .await?;
    bulk_insert_dataset_membership(tx, seed.draft.dataset_version_id, seed.dataset_cases).await?;
    insert_run_create(tx, seed.run_id, seed.draft).await?;
    bulk_insert_run_chunks(tx, seed.run_id, seed.draft.dataset_version_id, chunks).await
}

/// Chooses initial execution placements for the run shards used by a new run.
///
/// The returned assignments are persisted to `shard_placements`; runtime
/// routing reads those stored rows instead of recomputing this policy.
pub(crate) async fn assign_run_shard_placements(
    database: &database::Db,
    chunks: &[RunChunkDraft],
) -> anyhow::Result<Vec<RunShardPlacementAssignment>> {
    let run_shards = chunks
        .iter()
        .map(|chunk| chunk.run_shard)
        .collect::<BTreeSet<_>>();

    let aliases = match database.shard_assignment_policy() {
        ShardAssignmentPolicy::SingleDefault => {
            vec![database.default_execution_database_alias().to_string()]
        }
        ShardAssignmentPolicy::SpreadActive => {
            let mut aliases = database.active_shard_capable_database_aliases().await?;
            if aliases.is_empty() {
                anyhow::bail!("no active shard-capable database placements are configured");
            }
            if let Some(default_idx) = aliases
                .iter()
                .position(|alias| alias == database.default_execution_database_alias())
            {
                aliases.swap(0, default_idx);
            }
            aliases
        }
    };

    Ok(assign_run_shards_to_aliases(&run_shards, &aliases))
}

pub(crate) fn assign_run_shards_to_aliases(
    run_shards: &BTreeSet<i16>,
    aliases: &[String],
) -> Vec<RunShardPlacementAssignment> {
    if aliases.is_empty() {
        return Vec::new();
    }

    run_shards
        .iter()
        .enumerate()
        .map(|(idx, run_shard)| RunShardPlacementAssignment {
            run_shard: *run_shard,
            database_alias: aliases[idx % aliases.len()].clone(),
        })
        .collect()
}

fn group_chunks_by_assigned_alias(
    chunks: &[RunChunkDraft],
    assignments: &[RunShardPlacementAssignment],
) -> anyhow::Result<BTreeMap<String, Vec<RunChunkDraft>>> {
    let aliases_by_shard = assignments
        .iter()
        .map(|assignment| (assignment.run_shard, assignment.database_alias.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut grouped = BTreeMap::<String, Vec<RunChunkDraft>>::new();

    for chunk in chunks {
        let Some(alias) = aliases_by_shard.get(&chunk.run_shard) else {
            anyhow::bail!(
                "missing shard placement assignment for run_shard {}",
                chunk.run_shard
            );
        };
        grouped
            .entry(alias.clone())
            .or_default()
            .push(chunk.clone());
    }

    Ok(grouped)
}

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
                .push_bind(Json(row.input_payload.clone()))
                .push_bind(Json(row.expected_output.clone()))
                .push_bind(Json(row.context_payload.clone()))
                .push_bind(Json(row.tags.clone()))
                .push_bind(Json(row.metadata.clone()));
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
    assignments: &[RunShardPlacementAssignment],
) -> anyhow::Result<()> {
    if assignments.is_empty() {
        return Ok(());
    }

    let database_aliases = assignments
        .iter()
        .map(|assignment| assignment.database_alias.as_str())
        .collect::<BTreeSet<_>>();
    for database_alias in database_aliases {
        validate_active_shard_capable_placement(tx, database_alias).await?;
    }

    let mut query_builder = QueryBuilder::<Postgres>::new(
        "INSERT INTO shard_placements (run_id, run_shard, database_alias, status) ",
    );

    query_builder.push_values(assignments, |mut b, assignment| {
        b.push_bind(run_id)
            .push_bind(assignment.run_shard)
            .push_bind(&assignment.database_alias)
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
    async fn bulk_insert_case_blobs_preserves_json_null_context(pool: PgPool) {
        let case_blob = CaseBlobDraft {
            case_hash: format!("case-{}", Uuid::now_v7()),
            task_type: "classification".to_string(),
            case_group: None,
            input_payload: serde_json::json!({"text": "hello"}),
            expected_output: serde_json::Value::Null,
            context_payload: serde_json::Value::Null,
            tags: serde_json::json!([]),
            metadata: serde_json::json!({}),
        };

        let mut tx = pool.begin().await.unwrap();
        bulk_insert_case_blobs(&mut tx, std::slice::from_ref(&case_blob))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let context_payload = sqlx::query_scalar::<_, serde_json::Value>(
            r#"
            SELECT context_payload
            FROM case_blobs
            WHERE case_hash = $1
            "#,
        )
        .bind(&case_blob.case_hash)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(context_payload, serde_json::Value::Null);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn bulk_insert_shard_placements_inserts_distinct_active_shards(pool: PgPool) {
        let run_id = Uuid::now_v7();
        insert_minimal_run(&pool, run_id).await;

        let assignments = vec![
            RunShardPlacementAssignment {
                run_shard: 0,
                database_alias: "primary".to_string(),
            },
            RunShardPlacementAssignment {
                run_shard: 1,
                database_alias: "primary".to_string(),
            },
        ];
        let mut tx = pool.begin().await.unwrap();
        bulk_insert_shard_placements(&mut tx, run_id, &assignments)
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

        let assignments = vec![RunShardPlacementAssignment {
            run_shard: 0,
            database_alias: "primary".to_string(),
        }];
        let mut tx = pool.begin().await.unwrap();
        let error = bulk_insert_shard_placements(&mut tx, run_id, &assignments)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("not shard-capable"));
    }

    #[test]
    fn assign_run_shards_to_aliases_spreads_in_order() {
        let run_shards = [0, 1, 2, 3, 4].into_iter().collect::<BTreeSet<_>>();
        let aliases = vec!["primary".to_string(), "shard_001".to_string()];

        let assignments = assign_run_shards_to_aliases(&run_shards, &aliases);

        assert_eq!(
            assignments,
            vec![
                RunShardPlacementAssignment {
                    run_shard: 0,
                    database_alias: "primary".to_string(),
                },
                RunShardPlacementAssignment {
                    run_shard: 1,
                    database_alias: "shard_001".to_string(),
                },
                RunShardPlacementAssignment {
                    run_shard: 2,
                    database_alias: "primary".to_string(),
                },
                RunShardPlacementAssignment {
                    run_shard: 3,
                    database_alias: "shard_001".to_string(),
                },
                RunShardPlacementAssignment {
                    run_shard: 4,
                    database_alias: "primary".to_string(),
                },
            ]
        );
    }
}
