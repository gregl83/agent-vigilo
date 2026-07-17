//! Durable multi-database run creation workflow.
//!
//! Control storage records a non-dispatchable creation plan before any remote
//! execution database commits. Seed writes are idempotent, so a coordinator can
//! resume the same `run_id` after a process or database failure. Dispatch
//! cursors are created only when every selected placement is seeded.

use std::collections::BTreeMap;

use sqlx::{
    PgPool,
    Postgres,
    QueryBuilder,
};
use tracing::{
    debug,
    warn,
};
use uuid::Uuid;

use super::run_create::{
    self,
    RunShardPlacementAssignment,
};
use crate::{
    context::database,
    models::{
        case_blob::CaseBlobDraft,
        dataset_version_case::DatasetVersionCaseDraft,
        run::RunDraft,
        run_chunk::RunChunkDraft,
    },
};

pub(crate) const RUN_STATUS_CREATING: &str = "creating";
const RUN_STATUS_PENDING: &str = "pending";
const RUN_STATUS_FAILED: &str = "failed";
const INITIAL_CREATION_LEASE_SECONDS: i32 = 300;
const CREATION_RETRY_DELAY_SECONDS: i32 = 10;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RunCreationProgress {
    pub(crate) placement_count: i64,
    pub(crate) pending_placement_count: i64,
    pub(crate) seeded_placement_count: i64,
    pub(crate) failed_placement_count: i64,
    pub(crate) attempt_count: i64,
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunCreationOutcome {
    pub(crate) status: String,
    pub(crate) progress: RunCreationProgress,
    pub(crate) error_message: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RunCreationRecoveryStats {
    pub(crate) claimed: usize,
    pub(crate) completed: usize,
    pub(crate) deferred: usize,
    pub(crate) failed: usize,
}

struct RunSeedMaterial<'a> {
    draft: &'a RunDraft,
    case_blobs: &'a [CaseBlobDraft],
    dataset_cases: &'a [DatasetVersionCaseDraft],
}

struct OwnedRunSeedMaterial {
    draft: RunDraft,
    case_blobs: Vec<CaseBlobDraft>,
    dataset_cases: Vec<DatasetVersionCaseDraft>,
}

/// Persists and immediately attempts a recoverable run creation operation.
pub(crate) async fn create_run(
    database: &database::Db,
    run_id: Uuid,
    draft: &RunDraft,
    case_blobs: &[CaseBlobDraft],
    dataset_cases: &[DatasetVersionCaseDraft],
    chunks: &[RunChunkDraft],
    assignments: &[RunShardPlacementAssignment],
) -> anyhow::Result<RunCreationOutcome> {
    let owner_id = Uuid::now_v7();
    persist_creation_plan(
        database,
        owner_id,
        run_id,
        draft,
        case_blobs,
        dataset_cases,
        chunks,
        assignments,
    )
    .await?;

    let seed = RunSeedMaterial {
        draft,
        case_blobs,
        dataset_cases,
    };
    match resume_claimed_run(
        database,
        run_id,
        owner_id,
        seed,
        INITIAL_CREATION_LEASE_SECONDS,
    )
    .await
    {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            warn!(run_id = %run_id, error = %error, "run creation is durable but immediate seeding did not finish");
            Ok(RunCreationOutcome {
                status: RUN_STATUS_CREATING.to_string(),
                progress: select_creation_progress(database.control().await?, run_id).await?,
                error_message: Some(error.to_string()),
            })
        }
    }
}

/// Recovers a bounded set of expired `creating` runs.
pub(crate) async fn recover_creating_runs(
    database: &database::Db,
    coordinator_id: Uuid,
    lease_seconds: i32,
    limit: usize,
) -> anyhow::Result<RunCreationRecoveryStats> {
    let mut stats = RunCreationRecoveryStats::default();

    for _ in 0..limit {
        let Some(run_id) =
            claim_next_creating_run(database.control().await?, coordinator_id, lease_seconds)
                .await?
        else {
            break;
        };
        stats.claimed += 1;

        let owned_seed = match load_seed_material(database.control().await?, run_id).await {
            Ok(seed) => seed,
            Err(error) if run_create::is_seed_invariant_error(&error) => {
                fail_claimed_run(
                    database.control().await?,
                    run_id,
                    coordinator_id,
                    &error.to_string(),
                )
                .await?;
                stats.failed += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        let seed = RunSeedMaterial {
            draft: &owned_seed.draft,
            case_blobs: &owned_seed.case_blobs,
            dataset_cases: &owned_seed.dataset_cases,
        };

        match resume_claimed_run(database, run_id, coordinator_id, seed, lease_seconds).await {
            Ok(RunCreationOutcome { status, .. }) if status == RUN_STATUS_PENDING => {
                stats.completed += 1;
            }
            Ok(RunCreationOutcome { status, .. }) if status == RUN_STATUS_FAILED => {
                stats.failed += 1;
            }
            Ok(_) => stats.deferred += 1,
            Err(error) if run_create::is_seed_invariant_error(&error) => {
                fail_claimed_run(
                    database.control().await?,
                    run_id,
                    coordinator_id,
                    &error.to_string(),
                )
                .await?;
                stats.failed += 1;
            }
            Err(error) => return Err(error),
        }
    }

    Ok(stats)
}

/// Returns persisted placement progress for status and watch projections.
pub(crate) async fn select_creation_progress(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<RunCreationProgress> {
    let row = sqlx::query_as::<_, (i64, i64, i64, i64, i64, Option<String>)>(
        r#"
        SELECT
            COUNT(*)::bigint,
            COUNT(*) FILTER (WHERE status = 'pending')::bigint,
            COUNT(*) FILTER (WHERE status = 'seeded')::bigint,
            COUNT(*) FILTER (WHERE status = 'failed')::bigint,
            COALESCE(SUM(attempt_count), 0)::bigint,
            (
                SELECT latest.last_error
                FROM run_creation_placements latest
                WHERE latest.run_id = $1::uuid
                  AND latest.last_error IS NOT NULL
                ORDER BY latest.updated_at DESC, latest.database_alias
                LIMIT 1
            )
        FROM run_creation_placements
        WHERE run_id = $1::uuid
        "#,
    )
    .bind(run_id)
    .fetch_one(db)
    .await?;

    Ok(RunCreationProgress {
        placement_count: row.0,
        pending_placement_count: row.1,
        seeded_placement_count: row.2,
        failed_placement_count: row.3,
        attempt_count: row.4,
        last_error: row.5,
    })
}

#[allow(clippy::too_many_arguments)]
async fn persist_creation_plan(
    database: &database::Db,
    owner_id: Uuid,
    run_id: Uuid,
    draft: &RunDraft,
    case_blobs: &[CaseBlobDraft],
    dataset_cases: &[DatasetVersionCaseDraft],
    chunks: &[RunChunkDraft],
    assignments: &[RunShardPlacementAssignment],
) -> anyhow::Result<()> {
    let chunks_by_alias = run_create::group_chunks_by_assigned_alias(chunks, assignments)?;
    let control_db = database.control().await?;
    let mut tx = control_db.begin().await?;

    run_create::bulk_insert_case_blobs(&mut tx, case_blobs).await?;
    run_create::upsert_dataset_version(
        &mut tx,
        draft.dataset_version_id,
        draft.dataset_id,
        &draft.dataset_version,
    )
    .await?;
    run_create::bulk_insert_dataset_membership(&mut tx, draft.dataset_version_id, dataset_cases)
        .await?;
    run_create::insert_run_create(&mut tx, run_id, draft, RUN_STATUS_CREATING).await?;
    run_create::bulk_insert_shard_placements(&mut tx, run_id, assignments).await?;
    insert_creation_placements(&mut tx, run_id, &chunks_by_alias).await?;
    insert_creation_chunks(&mut tx, run_id, &chunks_by_alias).await?;

    if let Some(control_chunks) = chunks_by_alias.get(database.control_database_alias()) {
        run_create::bulk_insert_run_chunks(
            &mut tx,
            run_id,
            draft.dataset_version_id,
            control_chunks,
        )
        .await?;
        mark_control_placement_seeded(&mut tx, run_id, database.control_database_alias()).await?;
    }

    let claimed = sqlx::query(
        r#"
        UPDATE runs
        SET coordinator_id = $2::uuid,
            coordinator_leased_until = now() + make_interval(secs => $3),
            coordinator_heartbeat_at = now(),
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'creating'::run_status
        "#,
    )
    .bind(run_id)
    .bind(owner_id)
    .bind(INITIAL_CREATION_LEASE_SECONDS)
    .execute(tx.as_mut())
    .await?
    .rows_affected();
    if claimed != 1 {
        anyhow::bail!("failed to claim newly persisted run creation '{}'", run_id);
    }

    tx.commit().await?;
    Ok(())
}

async fn insert_creation_placements(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    chunks_by_alias: &BTreeMap<String, Vec<RunChunkDraft>>,
) -> anyhow::Result<()> {
    let mut query = QueryBuilder::<Postgres>::new(
        "INSERT INTO run_creation_placements (run_id, database_alias, status) ",
    );
    query.push_values(chunks_by_alias.keys(), |mut row, alias| {
        row.push_bind(run_id).push_bind(alias).push_bind("pending");
    });
    query.build().execute(tx.as_mut()).await?;
    Ok(())
}

async fn insert_creation_chunks(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    chunks_by_alias: &BTreeMap<String, Vec<RunChunkDraft>>,
) -> anyhow::Result<()> {
    let planned_chunks = chunks_by_alias
        .iter()
        .flat_map(|(alias, chunks)| chunks.iter().map(move |chunk| (alias, chunk)))
        .collect::<Vec<_>>();
    let mut query = QueryBuilder::<Postgres>::new(
        "INSERT INTO run_creation_chunks (run_id, database_alias, chunk_id, run_shard, profile_group_id, ordinal_start, ordinal_end) ",
    );
    query.push_values(planned_chunks, |mut row, (alias, chunk)| {
        row.push_bind(run_id)
            .push_bind(alias)
            .push_bind(chunk.chunk_id)
            .push_bind(chunk.run_shard)
            .push_bind(&chunk.profile_group_id)
            .push_bind(chunk.ordinal_start)
            .push_bind(chunk.ordinal_end);
    });
    query.build().execute(tx.as_mut()).await?;
    Ok(())
}

async fn mark_control_placement_seeded(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    database_alias: &str,
) -> anyhow::Result<()> {
    let updated = sqlx::query(
        r#"
        UPDATE run_creation_placements
        SET status = 'seeded',
            attempt_count = attempt_count + 1,
            seeded_at = now(),
            updated_at = now()
        WHERE run_id = $1::uuid
          AND database_alias = $2
          AND status = 'pending'
        "#,
    )
    .bind(run_id)
    .bind(database_alias)
    .execute(tx.as_mut())
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!(
            "control placement '{}' was not pending for run creation '{}'",
            database_alias,
            run_id
        );
    }
    Ok(())
}

async fn resume_claimed_run(
    database: &database::Db,
    run_id: Uuid,
    owner_id: Uuid,
    seed: RunSeedMaterial<'_>,
    lease_seconds: i32,
) -> anyhow::Result<RunCreationOutcome> {
    let control_db = database.control().await?;
    let pending_aliases = select_pending_placements(control_db, run_id, owner_id).await?;

    for database_alias in pending_aliases {
        start_placement_attempt(control_db, run_id, owner_id, &database_alias, lease_seconds)
            .await?;
        let chunks = match select_creation_chunks(control_db, run_id, &database_alias).await {
            Ok(chunks) => chunks,
            Err(error) if run_create::is_seed_invariant_error(&error) => {
                fail_placement_and_run(
                    control_db,
                    run_id,
                    owner_id,
                    &database_alias,
                    &error.to_string(),
                )
                .await?;
                return select_creation_outcome(control_db, run_id).await;
            }
            Err(error) => return Err(error),
        };
        let seed_result = seed_execution_placement(
            database,
            run_id,
            &database_alias,
            seed.draft,
            seed.case_blobs,
            seed.dataset_cases,
            &chunks,
        )
        .await;

        if let Err(error) = seed_result {
            if run_create::is_seed_invariant_error(&error) {
                fail_placement_and_run(
                    control_db,
                    run_id,
                    owner_id,
                    &database_alias,
                    &error.to_string(),
                )
                .await?;
            } else {
                defer_placement(
                    control_db,
                    run_id,
                    owner_id,
                    &database_alias,
                    &error.to_string(),
                )
                .await?;
            }
            return select_creation_outcome(control_db, run_id).await;
        }

        if let Err(error) =
            mark_placement_seeded(control_db, run_id, owner_id, &database_alias, lease_seconds)
                .await
        {
            defer_placement(
                control_db,
                run_id,
                owner_id,
                &database_alias,
                &error.to_string(),
            )
            .await?;
            return select_creation_outcome(control_db, run_id).await;
        }
    }

    finish_claimed_run(control_db, run_id, owner_id).await?;
    select_creation_outcome(control_db, run_id).await
}

async fn finish_claimed_run(db: &PgPool, run_id: Uuid, owner_id: Uuid) -> anyhow::Result<()> {
    if let Err(error) = activate_claimed_run(db, run_id, owner_id).await {
        if run_create::is_seed_invariant_error(&error) {
            fail_claimed_run(db, run_id, owner_id, &error.to_string()).await?;
        } else {
            defer_run(db, run_id, owner_id, &error.to_string()).await?;
        }
    }
    Ok(())
}

async fn select_pending_placements(
    db: &PgPool,
    run_id: Uuid,
    owner_id: Uuid,
) -> anyhow::Result<Vec<String>> {
    let aliases = sqlx::query_scalar::<_, String>(
        r#"
        SELECT creation.database_alias
        FROM run_creation_placements creation
        JOIN runs run ON run.id = creation.run_id
        WHERE creation.run_id = $1::uuid
          AND creation.status = 'pending'
          AND run.status = 'creating'::run_status
          AND run.coordinator_id = $2::uuid
        ORDER BY creation.database_alias
        "#,
    )
    .bind(run_id)
    .bind(owner_id)
    .fetch_all(db)
    .await?;
    Ok(aliases)
}

async fn start_placement_attempt(
    db: &PgPool,
    run_id: Uuid,
    owner_id: Uuid,
    database_alias: &str,
    lease_seconds: i32,
) -> anyhow::Result<()> {
    let updated = sqlx::query(
        r#"
        WITH owned_run AS (
            UPDATE runs
            SET coordinator_leased_until = now() + make_interval(secs => $4),
                coordinator_heartbeat_at = now(),
                updated_at = now()
            WHERE id = $1::uuid
              AND status = 'creating'::run_status
              AND coordinator_id = $2::uuid
            RETURNING id
        )
        UPDATE run_creation_placements creation
        SET attempt_count = attempt_count + 1,
            last_error = NULL,
            updated_at = now()
        FROM owned_run
        WHERE creation.run_id = owned_run.id
          AND creation.database_alias = $3
          AND creation.status = 'pending'
        "#,
    )
    .bind(run_id)
    .bind(owner_id)
    .bind(database_alias)
    .bind(lease_seconds.max(INITIAL_CREATION_LEASE_SECONDS))
    .execute(db)
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!(
            "run creation '{}' is no longer owned while seeding placement '{}'",
            run_id,
            database_alias
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn seed_execution_placement(
    database: &database::Db,
    run_id: Uuid,
    database_alias: &str,
    draft: &RunDraft,
    case_blobs: &[CaseBlobDraft],
    dataset_cases: &[DatasetVersionCaseDraft],
    chunks: &[RunChunkDraft],
) -> anyhow::Result<()> {
    let db = database.execution_database(database_alias).await?;
    let mut tx = db.begin().await?;
    run_create::bulk_insert_case_blobs(&mut tx, case_blobs).await?;
    run_create::upsert_dataset_version(
        &mut tx,
        draft.dataset_version_id,
        draft.dataset_id,
        &draft.dataset_version,
    )
    .await?;
    run_create::bulk_insert_dataset_membership(&mut tx, draft.dataset_version_id, dataset_cases)
        .await?;
    run_create::insert_run_create(&mut tx, run_id, draft, RUN_STATUS_PENDING).await?;
    run_create::bulk_insert_run_chunks(&mut tx, run_id, draft.dataset_version_id, chunks).await?;
    tx.commit().await?;
    Ok(())
}

async fn select_creation_chunks(
    db: &PgPool,
    run_id: Uuid,
    database_alias: &str,
) -> anyhow::Result<Vec<RunChunkDraft>> {
    let chunks = sqlx::query_as::<_, RunChunkDraft>(
        r#"
        SELECT
            chunk_id,
            run_shard,
            profile_group_id,
            ordinal_start,
            ordinal_end
        FROM run_creation_chunks
        WHERE run_id = $1::uuid
          AND database_alias = $2
        ORDER BY run_shard, ordinal_start, chunk_id
        "#,
    )
    .bind(run_id)
    .bind(database_alias)
    .fetch_all(db)
    .await?;
    if chunks.is_empty() {
        return Err(run_create::seed_invariant_error(format!(
            "run creation '{}' has no chunk plan for placement '{}'",
            run_id, database_alias
        )));
    }
    Ok(chunks)
}

async fn mark_placement_seeded(
    db: &PgPool,
    run_id: Uuid,
    owner_id: Uuid,
    database_alias: &str,
    lease_seconds: i32,
) -> anyhow::Result<()> {
    let updated = sqlx::query(
        r#"
        WITH owned_run AS (
            UPDATE runs
            SET coordinator_leased_until = now() + make_interval(secs => $4),
                coordinator_heartbeat_at = now(),
                updated_at = now()
            WHERE id = $1::uuid
              AND status = 'creating'::run_status
              AND coordinator_id = $2::uuid
            RETURNING id
        )
        UPDATE run_creation_placements creation
        SET status = 'seeded',
            last_error = NULL,
            seeded_at = now(),
            updated_at = now()
        FROM owned_run
        WHERE creation.run_id = owned_run.id
          AND creation.database_alias = $3
          AND creation.status = 'pending'
        "#,
    )
    .bind(run_id)
    .bind(owner_id)
    .bind(database_alias)
    .bind(lease_seconds.max(INITIAL_CREATION_LEASE_SECONDS))
    .execute(db)
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!(
            "run creation '{}' lost ownership before placement '{}' was recorded as seeded",
            run_id,
            database_alias
        );
    }
    Ok(())
}

async fn defer_placement(
    db: &PgPool,
    run_id: Uuid,
    owner_id: Uuid,
    database_alias: &str,
    error: &str,
) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    let updated = sqlx::query(
        r#"
        WITH owned_run AS (
            UPDATE runs
            SET error_message = $4,
                coordinator_leased_until = now() + make_interval(secs => $5),
                coordinator_heartbeat_at = now(),
                updated_at = now()
            WHERE id = $1::uuid
              AND status = 'creating'::run_status
              AND coordinator_id = $2::uuid
            RETURNING id
        )
        UPDATE run_creation_placements creation
        SET last_error = $4,
            updated_at = now()
        FROM owned_run
        WHERE creation.run_id = owned_run.id
          AND creation.database_alias = $3
          AND creation.status = 'pending'
        "#,
    )
    .bind(run_id)
    .bind(owner_id)
    .bind(database_alias)
    .bind(error)
    .bind(CREATION_RETRY_DELAY_SECONDS)
    .execute(tx.as_mut())
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!(
            "run creation '{}' lost ownership while deferring placement '{}'",
            run_id,
            database_alias
        );
    }
    tx.commit().await?;
    warn!(run_id = %run_id, database_alias, error, "deferred run creation placement for retry");
    Ok(())
}

async fn defer_run(db: &PgPool, run_id: Uuid, owner_id: Uuid, error: &str) -> anyhow::Result<()> {
    let updated = sqlx::query(
        r#"
        UPDATE runs
        SET error_message = $3,
            coordinator_leased_until = now() + make_interval(secs => $4),
            coordinator_heartbeat_at = now(),
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'creating'::run_status
          AND coordinator_id = $2::uuid
        "#,
    )
    .bind(run_id)
    .bind(owner_id)
    .bind(error)
    .bind(CREATION_RETRY_DELAY_SECONDS)
    .execute(db)
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!(
            "run creation '{}' lost ownership while deferring activation",
            run_id
        );
    }
    warn!(run_id = %run_id, error, "deferred run creation activation for retry");
    Ok(())
}

async fn fail_placement_and_run(
    db: &PgPool,
    run_id: Uuid,
    owner_id: Uuid,
    database_alias: &str,
    error: &str,
) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    let placement_updated = sqlx::query(
        r#"
        UPDATE run_creation_placements
        SET status = 'failed',
            last_error = $4,
            updated_at = now()
        WHERE run_id = $1::uuid
          AND database_alias = $3
          AND status = 'pending'
          AND EXISTS (
              SELECT 1
              FROM runs
              WHERE id = $1::uuid
                AND status = 'creating'::run_status
                AND coordinator_id = $2::uuid
          )
        "#,
    )
    .bind(run_id)
    .bind(owner_id)
    .bind(database_alias)
    .bind(error)
    .execute(tx.as_mut())
    .await?
    .rows_affected();
    if placement_updated != 1 {
        anyhow::bail!(
            "run creation '{}' lost ownership before placement '{}' could fail",
            run_id,
            database_alias
        );
    }
    mark_run_failed(&mut tx, run_id, owner_id, error).await?;
    tx.commit().await?;
    Ok(())
}

async fn fail_claimed_run(
    db: &PgPool,
    run_id: Uuid,
    owner_id: Uuid,
    error: &str,
) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        r#"
        UPDATE run_creation_placements
        SET status = 'failed',
            last_error = $3,
            updated_at = now()
        WHERE run_id = $1::uuid
          AND status = 'pending'
          AND EXISTS (
              SELECT 1
              FROM runs
              WHERE id = $1::uuid
                AND status = 'creating'::run_status
                AND coordinator_id = $2::uuid
          )
        "#,
    )
    .bind(run_id)
    .bind(owner_id)
    .bind(error)
    .execute(tx.as_mut())
    .await?;
    mark_run_failed(&mut tx, run_id, owner_id, error).await?;
    tx.commit().await?;
    Ok(())
}

async fn mark_run_failed(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    owner_id: Uuid,
    error: &str,
) -> anyhow::Result<()> {
    let updated = sqlx::query(
        r#"
        UPDATE runs
        SET status = 'failed'::run_status,
            error_message = $3,
            coordinator_id = NULL,
            coordinator_leased_until = NULL,
            coordinator_heartbeat_at = NULL,
            completed_at = now(),
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'creating'::run_status
          AND coordinator_id = $2::uuid
        "#,
    )
    .bind(run_id)
    .bind(owner_id)
    .bind(error)
    .execute(tx.as_mut())
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!(
            "run creation '{}' lost ownership before failure was recorded",
            run_id
        );
    }
    Ok(())
}

async fn activate_claimed_run(db: &PgPool, run_id: Uuid, owner_id: Uuid) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    let (total, pending, failed) = sqlx::query_as::<_, (i64, i64, i64)>(
        r#"
        SELECT
            COUNT(*)::bigint,
            COUNT(*) FILTER (WHERE status = 'pending')::bigint,
            COUNT(*) FILTER (WHERE status = 'failed')::bigint
        FROM run_creation_placements
        WHERE run_id = $1::uuid
        "#,
    )
    .bind(run_id)
    .fetch_one(tx.as_mut())
    .await?;
    if total == 0 {
        let error = run_create::seed_invariant_error(format!(
            "run creation '{}' has no placement ledger rows",
            run_id
        ));
        tx.rollback().await?;
        return Err(error);
    }
    if pending > 0 || failed > 0 {
        tx.rollback().await?;
        return Ok(());
    }

    let chunks = sqlx::query_as::<_, RunChunkDraft>(
        r#"
        SELECT
            chunk_id,
            run_shard,
            profile_group_id,
            ordinal_start,
            ordinal_end
        FROM run_creation_chunks
        WHERE run_id = $1::uuid
        ORDER BY run_shard, ordinal_start, chunk_id
        "#,
    )
    .bind(run_id)
    .fetch_all(tx.as_mut())
    .await?;
    if chunks.is_empty() {
        let error = run_create::seed_invariant_error(format!(
            "run creation '{}' has no persisted chunk plan",
            run_id
        ));
        tx.rollback().await?;
        return Err(error);
    }
    run_create::bulk_insert_run_shard_dispatch_cursors(&mut tx, run_id, &chunks).await?;

    let activated = sqlx::query(
        r#"
        UPDATE runs
        SET status = 'pending'::run_status,
            error_message = NULL,
            coordinator_id = NULL,
            coordinator_leased_until = NULL,
            coordinator_heartbeat_at = NULL,
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'creating'::run_status
          AND coordinator_id = $2::uuid
        "#,
    )
    .bind(run_id)
    .bind(owner_id)
    .execute(tx.as_mut())
    .await?
    .rows_affected();
    if activated != 1 {
        anyhow::bail!("run creation '{}' lost ownership before activation", run_id);
    }

    sqlx::query("DELETE FROM run_creation_chunks WHERE run_id = $1::uuid")
        .bind(run_id)
        .execute(tx.as_mut())
        .await?;
    tx.commit().await?;
    debug!(run_id = %run_id, "activated fully seeded run creation");
    Ok(())
}

async fn claim_next_creating_run(
    db: &PgPool,
    owner_id: Uuid,
    lease_seconds: i32,
) -> anyhow::Result<Option<Uuid>> {
    let run_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        WITH candidate AS (
            SELECT id
            FROM runs
            WHERE status = 'creating'::run_status
              AND (
                  coordinator_leased_until IS NULL
                  OR coordinator_leased_until < now()
              )
            ORDER BY created_at, id
            FOR UPDATE SKIP LOCKED
            LIMIT 1
        )
        UPDATE runs run
        SET coordinator_id = $1::uuid,
            coordinator_leased_until = now() + make_interval(secs => $2),
            coordinator_heartbeat_at = now(),
            updated_at = now()
        FROM candidate
        WHERE run.id = candidate.id
        RETURNING run.id
        "#,
    )
    .bind(owner_id)
    .bind(lease_seconds.max(INITIAL_CREATION_LEASE_SECONDS))
    .fetch_optional(db)
    .await?;
    Ok(run_id)
}

async fn load_seed_material(db: &PgPool, run_id: Uuid) -> anyhow::Result<OwnedRunSeedMaterial> {
    let draft = sqlx::query_as::<_, RunDraft>(
        r#"
        SELECT
            run_key,
            name,
            description,
            dataset_id,
            dataset_version,
            dataset_version_id,
            evaluation_profile_id,
            evaluation_profile_version,
            profile_version_id,
            profile_hash,
            aggregation_policy_id,
            aggregation_policy_version,
            aggregation_policy_hash,
            agent_provider,
            agent_name,
            agent_version,
            prompt_config_id,
            prompt_config_version,
            config_snapshot,
            expected_execution_count
        FROM runs
        WHERE id = $1::uuid
          AND status = 'creating'::run_status
        "#,
    )
    .bind(run_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        run_create::seed_invariant_error(format!(
            "creating run '{}' is missing its control run definition",
            run_id
        ))
    })?;

    let dataset_cases = sqlx::query_as::<_, DatasetVersionCaseDraft>(
        r#"
        SELECT case_id, case_ordinal, case_hash
        FROM dataset_version_cases
        WHERE dataset_version_id = $1::uuid
        ORDER BY case_ordinal, case_id
        "#,
    )
    .bind(draft.dataset_version_id)
    .fetch_all(db)
    .await?;
    if dataset_cases.is_empty() {
        return Err(run_create::seed_invariant_error(format!(
            "creating run '{}' has no canonical dataset membership",
            run_id
        )));
    }

    let case_blobs = sqlx::query_as::<_, CaseBlobDraft>(
        r#"
        SELECT DISTINCT ON (blob.case_hash)
            blob.case_hash,
            blob.task_type,
            blob.case_group,
            blob.input_payload,
            blob.expected_output,
            blob.context_payload,
            blob.tags,
            blob.metadata
        FROM dataset_version_cases membership
        JOIN case_blobs blob ON blob.case_hash = membership.case_hash
        WHERE membership.dataset_version_id = $1::uuid
        ORDER BY blob.case_hash
        "#,
    )
    .bind(draft.dataset_version_id)
    .fetch_all(db)
    .await?;
    if case_blobs.is_empty() {
        return Err(run_create::seed_invariant_error(format!(
            "creating run '{}' has no canonical case blobs",
            run_id
        )));
    }

    Ok(OwnedRunSeedMaterial {
        draft,
        case_blobs,
        dataset_cases,
    })
}

async fn select_creation_outcome(db: &PgPool, run_id: Uuid) -> anyhow::Result<RunCreationOutcome> {
    let (status, error_message) = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status::text, error_message FROM runs WHERE id = $1::uuid",
    )
    .bind(run_id)
    .fetch_one(db)
    .await?;
    Ok(RunCreationOutcome {
        status,
        progress: select_creation_progress(db, run_id).await?,
        error_message,
    })
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    async fn insert_creation_fixture(pool: &PgPool, status: &str) -> (Uuid, Uuid) {
        let run_id = Uuid::now_v7();
        let owner_id = Uuid::now_v7();
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        let chunk_id = Uuid::now_v7();

        sqlx::query(
            "INSERT INTO dataset_versions (dataset_version_id, dataset_id, dataset_version) VALUES ($1, $2, 'test')",
        )
        .bind(dataset_version_id)
        .bind(dataset_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO runs (
                id, run_key, dataset_id, dataset_version_id, dataset_version,
                evaluation_profile_id, evaluation_profile_version,
                profile_version_id, profile_hash,
                aggregation_policy_id, aggregation_policy_version,
                aggregation_policy_hash, agent_provider, agent_name,
                prompt_config_id, prompt_config_version,
                expected_execution_count, status,
                coordinator_id, coordinator_leased_until
            )
            VALUES (
                $1, $2, $3, $4, 'test',
                'profile', '1.0.0', 'profile-version', 'profile-hash',
                'aggregation', '1.0.0', 'aggregation-hash',
                'test', 'agent', 'prompt', '1.0.0', 1,
                'creating'::run_status, $5, now() + interval '5 minutes'
            )
            "#,
        )
        .bind(run_id)
        .bind(format!("run-{run_id}"))
        .bind(dataset_id)
        .bind(dataset_version_id)
        .bind(owner_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO run_creation_placements (
                run_id, database_alias, status, seeded_at
            )
            VALUES (
                $1, 'primary', $2,
                CASE WHEN $2 = 'seeded' THEN now() ELSE NULL END
            )
            "#,
        )
        .bind(run_id)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO run_creation_chunks (
                run_id, database_alias, chunk_id, run_shard,
                profile_group_id, ordinal_start, ordinal_end
            )
            VALUES ($1, 'primary', $2, 0, 'default', 0, 1)
            "#,
        )
        .bind(run_id)
        .bind(chunk_id)
        .execute(pool)
        .await
        .unwrap();

        (run_id, owner_id)
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn activation_requires_every_placement_to_be_seeded(pool: PgPool) {
        let (run_id, owner_id) = insert_creation_fixture(&pool, "pending").await;

        activate_claimed_run(&pool, run_id, owner_id).await.unwrap();

        let status =
            sqlx::query_scalar::<_, String>("SELECT status::text FROM runs WHERE id = $1::uuid")
                .bind(run_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let cursor_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::bigint FROM run_shard_dispatch_cursors WHERE run_id = $1::uuid",
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(status, RUN_STATUS_CREATING);
        assert_eq!(cursor_count, 0);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn activation_creates_cursors_once_and_clears_chunk_plan(pool: PgPool) {
        let (run_id, owner_id) = insert_creation_fixture(&pool, "seeded").await;

        activate_claimed_run(&pool, run_id, owner_id).await.unwrap();

        let status =
            sqlx::query_scalar::<_, String>("SELECT status::text FROM runs WHERE id = $1::uuid")
                .bind(run_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let cursor_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::bigint FROM run_shard_dispatch_cursors WHERE run_id = $1::uuid",
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let plan_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::bigint FROM run_creation_chunks WHERE run_id = $1::uuid",
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(status, RUN_STATUS_PENDING);
        assert_eq!(cursor_count, 1);
        assert_eq!(plan_count, 0);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn activation_retry_rolls_back_partial_control_writes(pool: PgPool) {
        let (run_id, owner_id) = insert_creation_fixture(&pool, "seeded").await;
        sqlx::query(
            r#"
            CREATE FUNCTION fail_run_creation_activation()
            RETURNS trigger
            LANGUAGE plpgsql
            AS $$
            BEGIN
                IF OLD.status = 'creating' AND NEW.status = 'pending' THEN
                    RAISE EXCEPTION 'injected activation failure';
                END IF;
                RETURN NEW;
            END;
            $$
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            CREATE TRIGGER fail_run_creation_activation
            BEFORE UPDATE ON runs
            FOR EACH ROW
            EXECUTE FUNCTION fail_run_creation_activation()
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        finish_claimed_run(&pool, run_id, owner_id).await.unwrap();
        let (cursor_count, plan_count, error_message) =
            sqlx::query_as::<_, (i64, i64, Option<String>)>(
                r#"
                SELECT
                    (SELECT COUNT(*)::bigint FROM run_shard_dispatch_cursors WHERE run_id = $1::uuid),
                    (SELECT COUNT(*)::bigint FROM run_creation_chunks WHERE run_id = $1::uuid),
                    error_message
                FROM runs
                WHERE id = $1::uuid
                "#,
            )
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(cursor_count, 0);
        assert_eq!(plan_count, 1);
        assert!(
            error_message
                .as_deref()
                .is_some_and(|message| message.contains("injected activation failure"))
        );

        sqlx::query("DROP TRIGGER fail_run_creation_activation ON runs")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DROP FUNCTION fail_run_creation_activation()")
            .execute(&pool)
            .await
            .unwrap();
        finish_claimed_run(&pool, run_id, owner_id).await.unwrap();

        let (cursor_count, error_message) = sqlx::query_as::<_, (i64, Option<String>)>(
            r#"
            SELECT
                (SELECT COUNT(*)::bigint FROM run_shard_dispatch_cursors WHERE run_id = $1::uuid),
                error_message
            FROM runs
            WHERE id = $1::uuid
            "#,
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cursor_count, 1);
        assert_eq!(error_message, None);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn terminal_creation_failure_never_creates_dispatch_cursors(pool: PgPool) {
        let (run_id, owner_id) = insert_creation_fixture(&pool, "pending").await;

        fail_claimed_run(&pool, run_id, owner_id, "immutable seed mismatch")
            .await
            .unwrap();

        let (run_status, placement_status, cursor_count) =
            sqlx::query_as::<_, (String, String, i64)>(
                r#"
            SELECT
                run.status::text,
                creation.status,
                (
                    SELECT COUNT(*)::bigint
                    FROM run_shard_dispatch_cursors cursor
                    WHERE cursor.run_id = run.id
                )
            FROM runs run
            JOIN run_creation_placements creation ON creation.run_id = run.id
            WHERE run.id = $1::uuid
            "#,
            )
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(run_status, RUN_STATUS_FAILED);
        assert_eq!(placement_status, "failed");
        assert_eq!(cursor_count, 0);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn expired_creation_lease_can_be_reclaimed(pool: PgPool) {
        let (run_id, first_owner) = insert_creation_fixture(&pool, "pending").await;
        let second_owner = Uuid::now_v7();

        assert_eq!(
            claim_next_creating_run(&pool, second_owner, 60)
                .await
                .unwrap(),
            None
        );
        sqlx::query(
            "UPDATE runs SET coordinator_leased_until = now() - interval '1 second' WHERE id = $1::uuid",
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(
            claim_next_creating_run(&pool, second_owner, 60)
                .await
                .unwrap(),
            Some(run_id)
        );
        assert_ne!(first_owner, second_owner);
    }
}
