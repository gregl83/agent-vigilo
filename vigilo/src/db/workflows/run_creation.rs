//! Durable multi-database run creation workflow.
//!
//! The control database records a non-dispatchable creation plan before any remote
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

use super::{
    local_shard_admission::{
        LocalShardAdmissionDraft,
        LocalShardAdmissionState,
        upsert_local_shard_admission,
    },
    run_create::{
        self,
        RunShardPlacementAssignment,
    },
};
use crate::{
    context::database,
    db::workflows::case_projection,
    models::{
        case_blob::CaseBlobDraft,
        dataset_version_case::DatasetVersionCaseDraft,
        run::RunDraft,
        run_chunk::RunChunkDraft,
        run_shard_case::RunShardCaseDraft,
    },
};

pub(crate) const RUN_STATUS_CREATING: &str = "creating";
const RUN_STATUS_PENDING: &str = "pending";
const RUN_STATUS_FAILED: &str = "failed";
const INITIAL_CREATION_LEASE_SECONDS: i32 = 300;
const CREATION_RETRY_DELAY_SECONDS: i32 = 10;
pub(crate) const DEFAULT_CASE_BATCH_SIZE: usize = 1_000;
pub(crate) const DEFAULT_CASE_PAGE_BUDGET: usize = 64;

/// Bounded paging policy shared by immediate and coordinator run creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) case_batch_size: usize,
    pub(crate) case_page_budget: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            case_batch_size: DEFAULT_CASE_BATCH_SIZE,
            case_page_budget: DEFAULT_CASE_PAGE_BUDGET,
        }
    }
}

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

/// Immutable material persisted and seeded for one new run.
pub(crate) struct RunCreationRequest<'a> {
    pub(crate) run_id: Uuid,
    pub(crate) draft: &'a RunDraft,
    pub(crate) case_blobs: &'a [CaseBlobDraft],
    pub(crate) dataset_cases: &'a [DatasetVersionCaseDraft],
    pub(crate) chunks: &'a [RunChunkDraft],
    pub(crate) assignments: &'a [RunShardPlacementAssignment],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RunCreationRecoveryStats {
    pub(crate) claimed_runs: usize,
    pub(crate) completed_runs: usize,
    pub(crate) deferred_runs: usize,
    pub(crate) failed_runs: usize,
}

struct RunSeedMaterial<'a> {
    draft: &'a RunDraft,
}

struct OwnedRunSeedMaterial {
    draft: RunDraft,
}

#[derive(Debug, sqlx::FromRow)]
struct PlacementSeedProgress {
    expected_case_count: i64,
    seeded_case_count: i64,
    last_seeded_case_ordinal: Option<i32>,
    case_projection_hash: String,
}

/// Persists and immediately attempts a recoverable run creation operation.
pub(crate) async fn create_run(
    database_router: &database::DatabaseRouter,
    config: Config,
    request: RunCreationRequest<'_>,
) -> anyhow::Result<RunCreationOutcome> {
    let RunCreationRequest {
        run_id,
        draft,
        case_blobs,
        dataset_cases,
        chunks,
        assignments,
    } = request;
    let owner_id = Uuid::now_v7();
    persist_creation_plan(
        database_router,
        owner_id,
        run_id,
        draft,
        case_blobs,
        dataset_cases,
        chunks,
        assignments,
    )
    .await?;

    let seed = RunSeedMaterial { draft };
    match resume_claimed_run(
        database_router,
        config,
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
                progress: select_creation_progress(database_router.control().await?, run_id)
                    .await?,
                error_message: Some(error.to_string()),
            })
        }
    }
}

/// Recovers a bounded set of expired `creating` runs.
pub(crate) async fn recover_creating_runs(
    database_router: &database::DatabaseRouter,
    config: Config,
    coordinator_id: Uuid,
    lease_seconds: i32,
    limit: usize,
) -> anyhow::Result<RunCreationRecoveryStats> {
    let mut stats = RunCreationRecoveryStats::default();

    for _ in 0..limit {
        let Some(run_id) = claim_next_creating_run(
            database_router.control().await?,
            coordinator_id,
            lease_seconds,
        )
        .await?
        else {
            break;
        };
        stats.claimed_runs += 1;

        let owned_seed = match load_seed_material(database_router.control().await?, run_id).await {
            Ok(seed) => seed,
            Err(error) if run_create::is_seed_invariant_error(&error) => {
                fail_claimed_run(
                    database_router.control().await?,
                    run_id,
                    coordinator_id,
                    &error.to_string(),
                )
                .await?;
                stats.failed_runs += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        let seed = RunSeedMaterial {
            draft: &owned_seed.draft,
        };

        match resume_claimed_run(
            database_router,
            config,
            run_id,
            coordinator_id,
            seed,
            lease_seconds,
        )
        .await
        {
            Ok(RunCreationOutcome { status, .. }) if status == RUN_STATUS_PENDING => {
                stats.completed_runs += 1;
            }
            Ok(RunCreationOutcome { status, .. }) if status == RUN_STATUS_FAILED => {
                stats.failed_runs += 1;
            }
            Ok(_) => stats.deferred_runs += 1,
            Err(error) if run_create::is_seed_invariant_error(&error) => {
                fail_claimed_run(
                    database_router.control().await?,
                    run_id,
                    coordinator_id,
                    &error.to_string(),
                )
                .await?;
                stats.failed_runs += 1;
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
    database_router: &database::DatabaseRouter,
    owner_id: Uuid,
    run_id: Uuid,
    draft: &RunDraft,
    case_blobs: &[CaseBlobDraft],
    dataset_cases: &[DatasetVersionCaseDraft],
    chunks: &[RunChunkDraft],
    assignments: &[RunShardPlacementAssignment],
) -> anyhow::Result<()> {
    let chunks_by_alias = run_create::group_chunks_by_assigned_alias(chunks, assignments)?;
    let projections_by_alias = build_placement_projections(
        run_id,
        draft.dataset_version_id,
        dataset_cases,
        &chunks_by_alias,
    )?;
    let control_db = database_router.control().await?;
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
    insert_creation_placements(&mut tx, run_id, &projections_by_alias).await?;
    insert_creation_chunks(&mut tx, run_id, &chunks_by_alias).await?;

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

fn build_placement_projections(
    run_id: Uuid,
    dataset_version_id: Uuid,
    dataset_cases: &[DatasetVersionCaseDraft],
    chunks_by_alias: &BTreeMap<String, Vec<RunChunkDraft>>,
) -> anyhow::Result<BTreeMap<String, Vec<RunShardCaseDraft>>> {
    let mut projections = BTreeMap::new();
    let mut assigned_ordinals = std::collections::BTreeSet::new();
    for (alias, chunks) in chunks_by_alias {
        let projection = case_projection::project_cases_for_chunks(
            run_id,
            dataset_version_id,
            dataset_cases,
            chunks,
        )?;
        for row in &projection {
            if !assigned_ordinals.insert(row.case_ordinal) {
                anyhow::bail!(
                    "case ordinal {} is assigned to multiple execution placements",
                    row.case_ordinal
                );
            }
        }
        projections.insert(alias.clone(), projection);
    }
    if assigned_ordinals.len() != dataset_cases.len() {
        anyhow::bail!(
            "run creation assigns {} of {} canonical cases",
            assigned_ordinals.len(),
            dataset_cases.len()
        );
    }
    Ok(projections)
}

async fn insert_creation_placements(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    projections_by_alias: &BTreeMap<String, Vec<RunShardCaseDraft>>,
) -> anyhow::Result<()> {
    let mut query = QueryBuilder::<Postgres>::new(
        "INSERT INTO run_creation_placements \
         (run_id, database_alias, status, expected_case_count, case_projection_hash) ",
    );
    query.push_values(projections_by_alias, |mut row, (alias, projection)| {
        row.push_bind(run_id)
            .push_bind(alias)
            .push_bind("pending")
            .push_bind(projection.len() as i64)
            .push_bind(case_projection::projection_hash(projection));
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

async fn resume_claimed_run(
    database_router: &database::DatabaseRouter,
    config: Config,
    run_id: Uuid,
    owner_id: Uuid,
    seed: RunSeedMaterial<'_>,
    lease_seconds: i32,
) -> anyhow::Result<RunCreationOutcome> {
    let control_db = database_router.control().await?;
    let pending_aliases = select_pending_placements(control_db, run_id, owner_id).await?;

    for database_alias in pending_aliases {
        let database_permit = match database_router.acquire_database_operation(&database_alias) {
            Ok(permit) => permit,
            Err(open) => {
                debug!(
                    run_id = %run_id,
                    database_alias,
                    retry_after_ms = open.retry_after.as_millis() as u64,
                    "deferred run creation placement while database circuit is open"
                );
                yield_claimed_run(control_db, run_id, owner_id).await?;
                return select_creation_outcome(control_db, run_id).await;
            }
        };
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
            database_router,
            config,
            run_id,
            owner_id,
            &database_alias,
            seed.draft,
            &chunks,
            lease_seconds,
        )
        .await;

        match &seed_result {
            Ok(_) => {
                if matches!(
                    database_router.record_database_operation_success(database_permit),
                    Some(database::CircuitTransition::Closed)
                ) {
                    debug!(
                        run_id = %run_id,
                        database_alias,
                        "closed execution database circuit after successful creation probe"
                    );
                }
            }
            Err(error) => {
                let (_, transition) =
                    database_router.record_database_operation_error(database_permit, error);
                if let Some(database::CircuitTransition::Opened { retry_after }) = transition {
                    warn!(
                        run_id = %run_id,
                        database_alias,
                        retry_after_ms = retry_after.as_millis() as u64,
                        "opened execution database circuit after creation availability failures"
                    );
                }
            }
        }

        let placement_complete = match seed_result {
            Ok(complete) => complete,
            Err(error) => {
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
        };
        if !placement_complete {
            yield_claimed_run(control_db, run_id, owner_id).await?;
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

async fn yield_claimed_run(db: &PgPool, run_id: Uuid, owner_id: Uuid) -> anyhow::Result<()> {
    let updated = sqlx::query(
        r#"
        UPDATE runs
        SET coordinator_id = NULL,
            coordinator_leased_until = now() + make_interval(secs => $3),
            coordinator_heartbeat_at = NULL,
            error_message = NULL,
            updated_at = now()
        WHERE id = $1::uuid
          AND status = 'creating'::run_status
          AND coordinator_id = $2::uuid
        "#,
    )
    .bind(run_id)
    .bind(owner_id)
    .bind(CREATION_RETRY_DELAY_SECONDS)
    .execute(db)
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!(
            "run creation '{}' lost ownership while yielding its page budget",
            run_id
        );
    }
    Ok(())
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
    database_router: &database::DatabaseRouter,
    config: Config,
    run_id: Uuid,
    owner_id: Uuid,
    database_alias: &str,
    draft: &RunDraft,
    chunks: &[RunChunkDraft],
    lease_seconds: i32,
) -> anyhow::Result<bool> {
    let db = database_router.execution_database(database_alias).await?;
    let control_db = database_router.control().await?;
    let progress = select_placement_seed_progress(control_db, run_id, database_alias).await?;
    let batch_size = config.case_batch_size;
    let page_budget = config.case_page_budget;
    let mut acknowledged_count = progress.seeded_case_count;
    let mut acknowledged_ordinal = progress.last_seeded_case_ordinal;
    let mut reached_projection_end = false;
    for _ in 0..page_budget {
        let page_rows = select_projection_seed_page(
            control_db,
            run_id,
            database_alias,
            acknowledged_ordinal,
            batch_size,
        )
        .await?;
        if page_rows.is_empty() {
            reached_projection_end = true;
            break;
        }
        let page_blobs = select_projection_page_blobs(control_db, &page_rows).await?;

        let mut tx = db.begin().await?;
        run_create::bulk_insert_case_blobs(&mut tx, &page_blobs).await?;
        run_create::upsert_dataset_version(
            &mut tx,
            draft.dataset_version_id,
            draft.dataset_id,
            &draft.dataset_version,
        )
        .await?;
        let local_run_status = if database_alias == database_router.control_database_alias() {
            RUN_STATUS_CREATING
        } else {
            RUN_STATUS_PENDING
        };
        run_create::insert_run_create(&mut tx, run_id, draft, local_run_status).await?;
        case_projection::insert_projection_page(&mut tx, &page_rows).await?;
        tx.commit().await?;

        let last_ordinal = page_rows
            .last()
            .expect("projection pages are non-empty")
            .case_ordinal;
        acknowledge_projection_page(
            control_db,
            run_id,
            owner_id,
            database_alias,
            acknowledged_count,
            acknowledged_ordinal,
            page_rows.len() as i64,
            last_ordinal,
            lease_seconds,
        )
        .await?;
        acknowledged_count += page_rows.len() as i64;
        acknowledged_ordinal = Some(last_ordinal);
    }
    if acknowledged_count > progress.expected_case_count
        || (reached_projection_end && acknowledged_count != progress.expected_case_count)
    {
        return Err(run_create::seed_invariant_error(format!(
            "run creation '{}' placement '{}' acknowledged {} of {} planned cases",
            run_id, database_alias, acknowledged_count, progress.expected_case_count
        )));
    }
    if acknowledged_count < progress.expected_case_count {
        return Ok(false);
    }

    let mut placement_shards = chunks
        .iter()
        .map(|chunk| chunk.run_shard)
        .collect::<Vec<_>>();
    placement_shards.sort_unstable();
    placement_shards.dedup();
    let (stored_count, stored_hash) =
        case_projection::projection_fingerprint(&db, run_id, &placement_shards).await?;
    if stored_count != progress.expected_case_count || stored_hash != progress.case_projection_hash
    {
        return Err(run_create::seed_invariant_error(format!(
            "run creation '{}' placement '{}' projection verification failed",
            run_id, database_alias
        )));
    }

    let mut tx = db.begin().await?;
    for run_shard in placement_shards {
        upsert_local_shard_admission(
            &mut *tx,
            LocalShardAdmissionDraft {
                run_id,
                run_shard,
                database_alias: database_alias.to_string(),
                write_epoch: 1,
                state: LocalShardAdmissionState::Open,
                redirect_database_alias: None,
            },
        )
        .await?;
    }
    run_create::bulk_insert_run_chunks(&mut tx, run_id, draft.dataset_version_id, chunks).await?;
    tx.commit().await?;
    Ok(true)
}

async fn select_projection_seed_page(
    control_db: &PgPool,
    run_id: Uuid,
    database_alias: &str,
    after_ordinal: Option<i32>,
    limit: usize,
) -> anyhow::Result<Vec<RunShardCaseDraft>> {
    let rows = sqlx::query_as::<_, RunShardCaseDraft>(
        r#"
        SELECT
            plan.run_id,
            plan.run_shard,
            run.dataset_version_id,
            membership.case_id,
            membership.case_ordinal,
            membership.case_hash
        FROM run_creation_chunks plan
        JOIN runs run ON run.id = plan.run_id
        JOIN dataset_version_cases membership
          ON membership.dataset_version_id = run.dataset_version_id
         AND membership.case_ordinal >= plan.ordinal_start
         AND membership.case_ordinal < plan.ordinal_end
        WHERE plan.run_id = $1::uuid
          AND plan.database_alias = $2
          AND ($3::integer IS NULL OR membership.case_ordinal > $3)
        ORDER BY membership.case_ordinal, membership.case_id
        LIMIT $4
        "#,
    )
    .bind(run_id)
    .bind(database_alias)
    .bind(after_ordinal)
    .bind(limit as i64)
    .fetch_all(control_db)
    .await?;
    Ok(rows)
}

async fn select_projection_page_blobs(
    control_db: &PgPool,
    rows: &[RunShardCaseDraft],
) -> anyhow::Result<Vec<CaseBlobDraft>> {
    let expected_hashes = rows
        .iter()
        .map(|row| row.case_hash.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let hashes = expected_hashes.iter().copied().collect::<Vec<_>>();
    let blobs = sqlx::query_as::<_, CaseBlobDraft>(
        r#"
        SELECT
            case_hash, task_type, case_group, input_payload,
            expected_output, context_payload, tags, metadata
        FROM case_blobs
        WHERE case_hash = ANY($1::text[])
        ORDER BY case_hash
        "#,
    )
    .bind(&hashes)
    .fetch_all(control_db)
    .await?;
    if blobs.len() != expected_hashes.len() {
        return Err(run_create::seed_invariant_error(
            "projection page references a missing canonical case blob",
        ));
    }
    Ok(blobs)
}

async fn select_placement_seed_progress(
    db: &PgPool,
    run_id: Uuid,
    database_alias: &str,
) -> anyhow::Result<PlacementSeedProgress> {
    sqlx::query_as::<_, PlacementSeedProgress>(
        r#"
        SELECT expected_case_count, seeded_case_count,
               last_seeded_case_ordinal, case_projection_hash
        FROM run_creation_placements
        WHERE run_id = $1::uuid
          AND database_alias = $2
          AND status = 'pending'
        "#,
    )
    .bind(run_id)
    .bind(database_alias)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "run creation '{}' placement '{}' is no longer pending",
            run_id,
            database_alias
        )
    })
}

#[allow(clippy::too_many_arguments)]
async fn acknowledge_projection_page(
    db: &PgPool,
    run_id: Uuid,
    owner_id: Uuid,
    database_alias: &str,
    expected_count: i64,
    expected_ordinal: Option<i32>,
    page_count: i64,
    page_last_ordinal: i32,
    lease_seconds: i32,
) -> anyhow::Result<()> {
    let updated = sqlx::query(
        r#"
        WITH owned_run AS (
            UPDATE runs
            SET coordinator_leased_until = now() + make_interval(secs => $8),
                coordinator_heartbeat_at = now(),
                updated_at = now()
            WHERE id = $1::uuid
              AND status = 'creating'::run_status
              AND coordinator_id = $2::uuid
            RETURNING id
        )
        UPDATE run_creation_placements creation
        SET seeded_case_count = seeded_case_count + $6,
            last_seeded_case_ordinal = $7,
            updated_at = now()
        FROM owned_run
        WHERE creation.run_id = owned_run.id
          AND creation.database_alias = $3
          AND creation.status = 'pending'
          AND creation.seeded_case_count = $4
          AND creation.last_seeded_case_ordinal IS NOT DISTINCT FROM $5
        "#,
    )
    .bind(run_id)
    .bind(owner_id)
    .bind(database_alias)
    .bind(expected_count)
    .bind(expected_ordinal)
    .bind(page_count)
    .bind(page_last_ordinal)
    .bind(lease_seconds.max(INITIAL_CREATION_LEASE_SECONDS))
    .execute(db)
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!(
            "run creation '{}' lost ownership while acknowledging placement '{}' through ordinal {}",
            run_id,
            database_alias,
            page_last_ordinal
        );
    }
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
          AND creation.seeded_case_count = creation.expected_case_count
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

    Ok(OwnedRunSeedMaterial { draft })
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

    fn projection_case(ordinal: i32) -> DatasetVersionCaseDraft {
        DatasetVersionCaseDraft {
            case_id: Uuid::from_u128(ordinal as u128 + 1),
            case_ordinal: ordinal,
            case_hash: format!("hash-{ordinal}"),
        }
    }

    fn projection_chunk(run_shard: i16, start: i32, end: i32) -> RunChunkDraft {
        RunChunkDraft {
            chunk_id: Uuid::now_v7(),
            run_shard,
            profile_group_id: "default".to_string(),
            ordinal_start: start,
            ordinal_end: end,
        }
    }

    #[test]
    fn placement_projection_plan_covers_every_case_once() {
        let run_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        let cases = (0..4).map(projection_case).collect::<Vec<_>>();
        let chunks = BTreeMap::from([
            ("primary".to_string(), vec![projection_chunk(0, 0, 2)]),
            ("shard_001".to_string(), vec![projection_chunk(1, 2, 4)]),
        ]);

        let projections =
            build_placement_projections(run_id, dataset_version_id, &cases, &chunks).unwrap();

        assert_eq!(projections["primary"].len(), 2);
        assert_eq!(projections["shard_001"].len(), 2);
    }

    #[test]
    fn placement_projection_plan_rejects_unassigned_cases() {
        let error = build_placement_projections(
            Uuid::now_v7(),
            Uuid::now_v7(),
            &(0..3).map(projection_case).collect::<Vec<_>>(),
            &BTreeMap::from([("primary".to_string(), vec![projection_chunk(0, 0, 2)])]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("assigns 2 of 3"));
    }

    #[test]
    fn run_creation_config_has_bounded_defaults() {
        assert_eq!(
            Config::default(),
            Config {
                case_batch_size: 1_000,
                case_page_budget: 64,
            }
        );
    }

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
                run_id, database_alias, status, expected_case_count,
                seeded_case_count, last_seeded_case_ordinal,
                case_projection_hash, seeded_at
            )
            VALUES (
                $1, 'primary', $2, 1,
                CASE WHEN $2 = 'seeded' THEN 1 ELSE 0 END,
                CASE WHEN $2 = 'seeded' THEN 0 ELSE NULL END,
                'fixture-projection-hash',
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
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx creation tests"]
    async fn projection_seed_page_reads_only_persisted_placement_ranges(pool: PgPool) {
        let (run_id, _) = insert_creation_fixture(&pool, "pending").await;
        let dataset_version_id =
            sqlx::query_scalar::<_, Uuid>("SELECT dataset_version_id FROM runs WHERE id = $1")
                .bind(run_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        for ordinal in 0..3 {
            let case_id = Uuid::now_v7();
            let case_hash = format!("seed-page-{ordinal}");
            sqlx::query(
                "INSERT INTO case_blobs (case_hash, task_type, input_payload, expected_output) VALUES ($1, 'test', '{}'::jsonb, 'null'::jsonb)",
            )
            .bind(&case_hash)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO dataset_version_cases (dataset_version_id, case_id, case_ordinal, case_hash) VALUES ($1, $2, $3, $4)",
            )
            .bind(dataset_version_id)
            .bind(case_id)
            .bind(ordinal)
            .bind(case_hash)
            .execute(&pool)
            .await
            .unwrap();
        }

        let page = select_projection_seed_page(&pool, run_id, "primary", None, 10)
            .await
            .unwrap();
        let blobs = select_projection_page_blobs(&pool, &page).await.unwrap();
        let next_page = select_projection_seed_page(&pool, run_id, "primary", Some(0), 10)
            .await
            .unwrap();

        assert_eq!(page.len(), 1);
        assert_eq!(page[0].case_ordinal, 0);
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].case_hash, page[0].case_hash);
        assert!(next_page.is_empty());
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
