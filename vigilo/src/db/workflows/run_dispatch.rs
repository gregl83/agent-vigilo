//! Run dispatch workflow helpers.
//!
//! Coordinators use this module to start pending runs in control storage,
//! prepare execution-local run snapshots, and enqueue bounded windows of outbox
//! events that make chunks visible to workers. Dispatch cursors stay in control
//! storage and serialize route claims while execution storage owns chunk scans
//! and chunk-ready outbox events.

use std::collections::BTreeSet;

use sqlx::PgPool;
use uuid::Uuid;

use super::run_shard_summary;

/// Run projection returned after a coordinator dispatches one shard window.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct DispatchedRun {
    pub(crate) id: Uuid,
    pub(crate) run_key: String,
    pub(crate) run_shard: i16,
    pub(crate) chunk_events_enqueued: i64,
    pub(crate) chunks_marked_dispatched: i64,
    pub(crate) run_started_events_enqueued: i64,
}

/// Immutable run context copied from control storage before execution dispatch.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct DispatchRunSnapshot {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) run_key: String,
    pub(crate) dataset_id: Uuid,
    pub(crate) dataset_version_id: Uuid,
    pub(crate) dataset_version: String,
    pub(crate) evaluation_profile_id: String,
    pub(crate) evaluation_profile_version: String,
    pub(crate) profile_version_id: String,
    pub(crate) profile_hash: String,
    pub(crate) aggregation_policy_id: String,
    pub(crate) aggregation_policy_version: String,
    pub(crate) aggregation_policy_hash: String,
    pub(crate) agent_provider: String,
    pub(crate) agent_name: String,
    pub(crate) agent_version: Option<String>,
    pub(crate) prompt_config_id: String,
    pub(crate) prompt_config_version: String,
    pub(crate) config_snapshot: serde_json::Value,
    pub(crate) run_started_events_enqueued: i64,
}

/// Control-plane route selected for one dispatch attempt.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct DispatchRoute {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) database_alias: String,
    pub(crate) placement_status: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct DispatchedRunWindow {
    id: Uuid,
    run_key: String,
    run_shard: i16,
    chunk_events_enqueued: i64,
    chunks_marked_dispatched: i64,
    run_started_events_enqueued: i64,
    has_remaining_chunks: bool,
}

/// Counts returned after one expired chunk lease recovery pass.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ChunkLeaseRecoveryStats {
    pub(crate) recovered: i64,
    pub(crate) failed: i64,
}

/// Recovers expired worker chunk leases for prepared run snapshots.
///
/// Recovered chunks are moved back to `pending` and receive a fresh
/// `run.chunk.ready` event with a recovery-scoped dedupe key. Chunks that have
/// already reached the recovery limit are marked failed so finalization can
/// terminate the run instead of leaving it blocked by a dead lease forever.
///
/// Query behavior:
/// - Runs in one transaction so recovery and poison-chunk failure see a
///   consistent lease snapshot.
/// - First query locks a bounded oldest-expired set below `max_recoveries`,
///   resets those chunks to `pending`, increments recovery metadata, and emits
///   idempotent recovery-scoped chunk-ready events.
/// - Second query locks a bounded oldest-expired set already at the recovery
///   limit and marks them `failed`.
/// - `SKIP LOCKED` lets multiple coordinators run recovery without blocking
///   each other on the same chunk rows.
pub(crate) async fn recover_expired_chunk_leases(
    db: &PgPool,
    max_recoveries: i32,
    batch_size: i64,
) -> anyhow::Result<ChunkLeaseRecoveryStats> {
    let mut tx = db.begin().await?;

    // Query outline:
    //
    // expired         - recoverable leased chunks whose lease is past due.
    // recovered       - clear lease, increment recovery_count, return rows.
    // stale_recovered_attempts
    //                 - mark current running attempts stale before requeue.
    // recovery_events - publish a fresh, recovery-scoped chunk-ready event.
    let recovered = sqlx::query_scalar::<_, i64>(
        r#"
        WITH expired AS (
            SELECT
                rc.run_id,
                rc.run_shard,
                rc.id,
                rc.recovery_count + 1 AS next_recovery_count
            FROM run_chunks rc
            JOIN run_snapshots rs
              ON rs.run_id = rc.run_id
             AND rs.run_shard = rc.run_shard
            WHERE rc.status = 'leased'
              AND rc.leased_until < now()
              AND rc.recovery_count < $1
            ORDER BY rc.leased_until ASC, rc.run_id ASC, rc.id ASC
            FOR UPDATE OF rc SKIP LOCKED
            LIMIT $2
        ),
        recovered AS (
            UPDATE run_chunks rc
            SET status = 'pending',
                leased_until = NULL,
                recovery_count = expired.next_recovery_count,
                last_recovered_at = now(),
                updated_at = now()
            FROM expired
            WHERE rc.run_id = expired.run_id
              AND rc.run_shard = expired.run_shard
              AND rc.id = expired.id
            RETURNING rc.run_id, rc.run_shard, rc.id, rc.recovery_count
        ),
        stale_recovered_attempts AS (
            UPDATE execution_attempts ea
            SET status = 'stale'::attempt_status,
                error_message = COALESCE(
                    ea.error_message,
                    'attempt lease expired with recovered chunk'
                ),
                completed_at = COALESCE(ea.completed_at, now()),
                leased_until = NULL,
                updated_at = now()
            FROM recovered
            JOIN executions e
              ON e.run_id = recovered.run_id
             AND e.run_shard = recovered.run_shard
             AND e.chunk_id = recovered.id
            WHERE ea.run_id = recovered.run_id
              AND ea.run_shard = recovered.run_shard
              AND ea.execution_id = e.id
              AND ea.id = e.current_attempt_id
              AND ea.status = 'running'::attempt_status
            RETURNING ea.id
        ),
        recovery_events AS (
            INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload)
            SELECT
                'run.chunk.ready',
                'run',
                recovered.run_id,
                format(
                    'run:%s:chunk:%s:ready:recovery:%s',
                    recovered.run_id,
                    recovered.id,
                    recovered.recovery_count
                ),
                jsonb_build_object(
                    'run_id', recovered.run_id,
                    'run_shard', recovered.run_shard,
                    'chunk_id', recovered.id,
                    'recovery_count', recovered.recovery_count
                )
            FROM recovered
            ON CONFLICT (dedupe_key) DO NOTHING
            RETURNING id
        )
        SELECT COUNT(*)::bigint
        FROM recovered
        "#,
    )
    .bind(max_recoveries)
    .bind(batch_size)
    .fetch_one(&mut *tx)
    .await?;

    // Query outline:
    //
    // expired - leased chunks past due that have exhausted recovery attempts.
    // failed  - clear the dead lease and make the chunk terminal.
    // stale_failed_attempts
    //         - mark current running attempts stale with the failed chunk.
    let failed_rows = sqlx::query_as::<_, (Uuid, i16)>(
        r#"
        WITH expired AS (
            SELECT rc.run_id, rc.run_shard, rc.id
            FROM run_chunks rc
            JOIN run_snapshots rs
              ON rs.run_id = rc.run_id
             AND rs.run_shard = rc.run_shard
            WHERE rc.status = 'leased'
              AND rc.leased_until < now()
              AND rc.recovery_count >= $1
            ORDER BY rc.leased_until ASC, rc.run_id ASC, rc.id ASC
            FOR UPDATE OF rc SKIP LOCKED
            LIMIT $2
        ),
        failed AS (
            UPDATE run_chunks rc
            SET status = 'failed',
                leased_until = NULL,
                updated_at = now()
            FROM expired
            WHERE rc.run_id = expired.run_id
              AND rc.run_shard = expired.run_shard
              AND rc.id = expired.id
            RETURNING rc.run_id, rc.run_shard
        ),
        stale_failed_attempts AS (
            UPDATE execution_attempts ea
            SET status = 'stale'::attempt_status,
                error_message = COALESCE(
                    ea.error_message,
                    'attempt lease expired with failed chunk'
                ),
                completed_at = COALESCE(ea.completed_at, now()),
                leased_until = NULL,
                updated_at = now()
            FROM expired
            JOIN executions e
              ON e.run_id = expired.run_id
             AND e.run_shard = expired.run_shard
             AND e.chunk_id = expired.id
            WHERE ea.run_id = expired.run_id
              AND ea.run_shard = expired.run_shard
              AND ea.execution_id = e.id
              AND ea.id = e.current_attempt_id
              AND ea.status = 'running'::attempt_status
            RETURNING ea.id
        )
        SELECT run_id, run_shard
        FROM failed
        "#,
    )
    .bind(max_recoveries)
    .bind(batch_size)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;

    let failed_shards = failed_rows.iter().copied().collect::<BTreeSet<_>>();
    for (run_id, run_shard) in failed_shards {
        run_shard_summary::refresh_run_shard_summary(db, run_id, run_shard).await?;
    }

    Ok(ChunkLeaseRecoveryStats {
        recovered,
        failed: i64::try_from(failed_rows.len())?,
    })
}

/// Selects the next control-plane dispatch route.
///
/// This query does not claim execution rows. It chooses one open
/// `(run_id, run_shard)` cursor whose shard placement is active and whose
/// database placement can hold execution data. The execution dispatch query
/// then locks and advances that exact cursor on the resolved placement.
pub(crate) async fn select_next_dispatch_route(
    db: &PgPool,
) -> anyhow::Result<Option<DispatchRoute>> {
    let route = sqlx::query_as::<_, DispatchRoute>(
        r#"
        SELECT
            c.run_id,
            c.run_shard,
            sp.database_alias,
            sp.status AS placement_status
        FROM run_shard_dispatch_cursors c
        JOIN runs r
          ON r.id = c.run_id
        JOIN shard_placements sp
          ON sp.run_id = c.run_id
         AND sp.run_shard = c.run_shard
        JOIN database_placements dp
          ON dp.alias = sp.database_alias
        WHERE c.status = 'open'
          AND sp.status = 'active'
          AND dp.status = 'active'
          AND dp.role IN ('shard', 'control_and_shard')
          AND (
              r.status = 'running'::run_status
              OR (
                  r.status = 'pending'::run_status
                  AND (
                      r.coordinator_leased_until IS NULL
                      OR r.coordinator_leased_until < now()
                  )
              )
          )
        ORDER BY c.updated_at ASC, c.run_id ASC, c.run_shard ASC
        LIMIT 1
        "#,
    )
    .fetch_optional(db)
    .await?;

    Ok(route)
}

/// Counts currently dispatchable control-plane cursor rows.
///
/// This mirrors [`select_next_dispatch_route`] without ordering or row return
/// data. Coordinator structured logs use it as a backlog gauge for scale
/// monitoring.
pub(crate) async fn count_dispatch_cursor_backlog(db: &PgPool) -> anyhow::Result<i64> {
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM run_shard_dispatch_cursors c
        JOIN runs r
          ON r.id = c.run_id
        JOIN shard_placements sp
          ON sp.run_id = c.run_id
         AND sp.run_shard = c.run_shard
        JOIN database_placements dp
          ON dp.alias = sp.database_alias
        WHERE c.status = 'open'
          AND sp.status = 'active'
          AND dp.status = 'active'
          AND dp.role IN ('shard', 'control_and_shard')
          AND (
              r.status = 'running'::run_status
              OR (
                  r.status = 'pending'::run_status
                  AND (
                      r.coordinator_leased_until IS NULL
                      OR r.coordinator_leased_until < now()
                  )
              )
          )
        "#,
    )
    .fetch_one(db)
    .await?;

    Ok(count)
}

/// Starts a dispatchable run in control storage and returns its snapshot.
///
/// The returned data is copied into execution storage by
/// [`dispatch_routed_run_window`] before worker-visible chunk events are
/// inserted. `run.started` is a control-plane event, so it is emitted here.
pub(crate) async fn prepare_dispatch_run_snapshot(
    db: &PgPool,
    route: &DispatchRoute,
    coordinator_id: Uuid,
    lease_seconds: i32,
) -> anyhow::Result<Option<DispatchRunSnapshot>> {
    let snapshot = sqlx::query_as::<_, DispatchRunSnapshot>(
        r#"
        WITH candidate AS (
            SELECT
                id,
                status AS previous_status
            FROM runs
            WHERE id = $1::uuid
              AND (
                  status = 'running'::run_status
                  OR (
                      status = 'pending'::run_status
                      AND (
                          coordinator_leased_until IS NULL
                          OR coordinator_leased_until < now()
                      )
                  )
              )
            FOR UPDATE
        ),
        started_run AS (
            UPDATE runs r
            SET status = 'running'::run_status,
                coordinator_id = $3::uuid,
                coordinator_leased_until = now() + ($4::int * interval '1 second'),
                coordinator_heartbeat_at = now(),
                started_at = COALESCE(r.started_at, now()),
                dispatched_at = COALESCE(r.dispatched_at, now()),
                updated_at = now()
            FROM candidate
            WHERE r.id = candidate.id
              AND candidate.previous_status = 'pending'::run_status
              AND r.status = 'pending'::run_status
            RETURNING r.*
        ),
        selected_run AS (
            SELECT *
            FROM started_run
            UNION ALL
            SELECT r.*
            FROM runs r
            JOIN candidate
              ON candidate.id = r.id
            WHERE candidate.previous_status = 'running'::run_status
              AND r.status = 'running'::run_status
        ),
        started_event AS (
            INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload)
            SELECT
                'run.started',
                'run',
                selected_run.id,
                format('run:%s:started', selected_run.id),
                jsonb_build_object('run_id', selected_run.id)
            FROM selected_run
            JOIN candidate
              ON candidate.id = selected_run.id
            WHERE candidate.previous_status = 'pending'::run_status
            ON CONFLICT (dedupe_key) DO NOTHING
            RETURNING id
        )
        SELECT
            selected_run.id AS run_id,
            $2::smallint AS run_shard,
            selected_run.run_key,
            selected_run.dataset_id,
            selected_run.dataset_version_id,
            selected_run.dataset_version,
            selected_run.evaluation_profile_id,
            selected_run.evaluation_profile_version,
            selected_run.profile_version_id,
            selected_run.profile_hash,
            selected_run.aggregation_policy_id,
            selected_run.aggregation_policy_version,
            selected_run.aggregation_policy_hash,
            selected_run.agent_provider,
            selected_run.agent_name,
            selected_run.agent_version,
            selected_run.prompt_config_id,
            selected_run.prompt_config_version,
            selected_run.config_snapshot,
            (SELECT COUNT(*)::bigint FROM started_event) AS run_started_events_enqueued
        FROM selected_run
        "#,
    )
    .bind(route.run_id)
    .bind(route.run_shard)
    .bind(coordinator_id)
    .bind(lease_seconds)
    .fetch_optional(db)
    .await?;

    Ok(snapshot)
}

/// Claims one dispatchable run shard and enqueues a bounded chunk-ready window.
///
/// The caller first prepares a control-owned [`DispatchRunSnapshot`]. This
/// function copies that snapshot into execution storage, claims one open
/// control cursor, scans only that shard's pending execution chunks, and then
/// releases or drains the control cursor.
///
/// Query behavior:
/// - Upserts the execution-local snapshot before worker-visible events.
/// - Selects the exact open control cursor with `FOR UPDATE SKIP LOCKED`.
/// - Selects a bounded chunk window inside one `run_id + run_shard`, emits
///   idempotent `run.chunk.ready` events, and advances each chunk's
///   `dispatched_at` cursor in the same statement.
/// - Releases the control cursor if more undispatched chunks remain, or marks it
///   drained when that shard has no undispatched pending chunks left.
#[cfg(test)]
async fn dispatch_next_run_window(
    db: &PgPool,
    coordinator_id: Uuid,
    lease_seconds: i32,
    chunk_window_size: i64,
) -> anyhow::Result<Option<DispatchedRun>> {
    let Some(route) = select_next_dispatch_route(db).await? else {
        return Ok(None);
    };
    let Some(snapshot) =
        prepare_dispatch_run_snapshot(db, &route, coordinator_id, lease_seconds).await?
    else {
        return Ok(None);
    };

    dispatch_routed_run_window(db, db, chunk_window_size, &route, &snapshot).await
}

/// Claims and dispatches one exact run-shard route.
///
/// The route is selected from control storage before the caller resolves the
/// execution placement. This function keeps the shard-local dispatch statement
/// constrained to that routed `run_id + run_shard`.
pub(crate) async fn dispatch_routed_run_window(
    control_db: &PgPool,
    execution_db: &PgPool,
    chunk_window_size: i64,
    route: &DispatchRoute,
    snapshot: &DispatchRunSnapshot,
) -> anyhow::Result<Option<DispatchedRun>> {
    if snapshot.run_id != route.run_id || snapshot.run_shard != route.run_shard {
        anyhow::bail!(
            "dispatch snapshot route mismatch: route {} shard {}, snapshot {} shard {}",
            route.run_id,
            route.run_shard,
            snapshot.run_id,
            snapshot.run_shard
        );
    }

    let mut control_tx = control_db.begin().await?;
    let cursor_locked = sqlx::query_scalar::<_, i32>(
        r#"
        SELECT 1
        FROM run_shard_dispatch_cursors
        WHERE run_id = $1::uuid
          AND run_shard = $2
          AND status = 'open'
        FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(route.run_id)
    .bind(route.run_shard)
    .fetch_optional(&mut *control_tx)
    .await?
    .is_some();

    if !cursor_locked {
        control_tx.rollback().await?;
        return Ok(None);
    }

    // Query outline:
    //
    // snapshot_input    - control-owned run context for the selected route.
    // snapshot_upsert   - local execution snapshot prepared before dispatch.
    // claimed          - common run projection for the claimed shard cursor.
    // selected_chunks  - bounded undispatched pending chunk window for one shard.
    // chunk_events     - idempotent queue-visible chunk-ready ledger rows.
    // marked_chunks    - cursor update proving those chunks were dispatched.
    // remaining_chunks - tells the control cursor whether another pass is needed.
    let dispatched = sqlx::query_as::<_, DispatchedRunWindow>(
        r#"
        WITH snapshot_input AS (
            SELECT
                $2::uuid AS run_id,
                $3::smallint AS run_shard,
                $4::text AS run_key,
                $5::uuid AS dataset_id,
                $6::uuid AS dataset_version_id,
                $7::text AS dataset_version,
                $8::text AS evaluation_profile_id,
                $9::text AS evaluation_profile_version,
                $10::text AS profile_version_id,
                $11::text AS profile_hash,
                $12::text AS aggregation_policy_id,
                $13::text AS aggregation_policy_version,
                $14::text AS aggregation_policy_hash,
                $15::text AS agent_provider,
                $16::text AS agent_name,
                $17::text AS agent_version,
                $18::text AS prompt_config_id,
                $19::text AS prompt_config_version,
                $20::jsonb AS config_snapshot
        ),
        snapshot_upsert AS (
            INSERT INTO run_snapshots (
                run_id,
                run_shard,
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
                agent_version,
                prompt_config_id,
                prompt_config_version,
                config_snapshot,
                expected_execution_count
            )
            SELECT
                snapshot_input.run_id,
                snapshot_input.run_shard,
                snapshot_input.run_key,
                snapshot_input.dataset_id,
                snapshot_input.dataset_version_id,
                snapshot_input.dataset_version,
                snapshot_input.evaluation_profile_id,
                snapshot_input.evaluation_profile_version,
                snapshot_input.profile_version_id,
                snapshot_input.profile_hash,
                snapshot_input.aggregation_policy_id,
                snapshot_input.aggregation_policy_version,
                snapshot_input.aggregation_policy_hash,
                snapshot_input.agent_provider,
                snapshot_input.agent_name,
                snapshot_input.agent_version,
                snapshot_input.prompt_config_id,
                snapshot_input.prompt_config_version,
                snapshot_input.config_snapshot,
                COALESCE((
                    SELECT SUM(rc.ordinal_end - rc.ordinal_start)::int
                    FROM run_chunks rc
                    WHERE rc.run_id = snapshot_input.run_id
                      AND rc.run_shard = snapshot_input.run_shard
                ), 0)
            FROM snapshot_input
            ON CONFLICT (run_id, run_shard) DO UPDATE
            SET run_key = EXCLUDED.run_key,
                dataset_id = EXCLUDED.dataset_id,
                dataset_version_id = EXCLUDED.dataset_version_id,
                dataset_version = EXCLUDED.dataset_version,
                evaluation_profile_id = EXCLUDED.evaluation_profile_id,
                evaluation_profile_version = EXCLUDED.evaluation_profile_version,
                profile_version_id = EXCLUDED.profile_version_id,
                profile_hash = EXCLUDED.profile_hash,
                aggregation_policy_id = EXCLUDED.aggregation_policy_id,
                aggregation_policy_version = EXCLUDED.aggregation_policy_version,
                aggregation_policy_hash = EXCLUDED.aggregation_policy_hash,
                agent_provider = EXCLUDED.agent_provider,
                agent_name = EXCLUDED.agent_name,
                agent_version = EXCLUDED.agent_version,
                prompt_config_id = EXCLUDED.prompt_config_id,
                prompt_config_version = EXCLUDED.prompt_config_version,
                config_snapshot = EXCLUDED.config_snapshot,
                expected_execution_count = EXCLUDED.expected_execution_count,
                updated_at = now()
            RETURNING run_id, run_shard, run_key
        ),
        claimed AS (
            SELECT
                snapshot_upsert.run_id AS id,
                snapshot_upsert.run_key,
                snapshot_upsert.run_shard
            FROM snapshot_upsert
        ),
        selected_chunks AS (
            SELECT rc.run_id, rc.run_shard, rc.id
            FROM run_chunks rc
            JOIN claimed
              ON claimed.id = rc.run_id
             AND claimed.run_shard = rc.run_shard
            WHERE rc.status = 'pending'
              AND rc.dispatched_at IS NULL
            ORDER BY rc.ordinal_start ASC, rc.id ASC
            LIMIT $1
        ),
        chunk_events AS (
            INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload)
            SELECT
                'run.chunk.ready',
                'run',
                selected_chunks.run_id,
                format('run:%s:chunk:%s:ready', selected_chunks.run_id, selected_chunks.id),
                jsonb_build_object(
                    'run_id', selected_chunks.run_id,
                    'run_shard', selected_chunks.run_shard,
                    'chunk_id', selected_chunks.id
                )
            FROM selected_chunks
            ON CONFLICT (dedupe_key) DO NOTHING
            RETURNING id
        ),
        marked_chunks AS (
            UPDATE run_chunks rc
            SET dispatched_at = COALESCE(rc.dispatched_at, now()),
                updated_at = now()
            FROM selected_chunks
            WHERE rc.run_id = selected_chunks.run_id
              AND rc.run_shard = selected_chunks.run_shard
              AND rc.id = selected_chunks.id
            RETURNING rc.id
        ),
        remaining_chunks AS (
            SELECT EXISTS (
                SELECT 1
                FROM run_chunks rc
                JOIN claimed
                  ON claimed.id = rc.run_id
                 AND claimed.run_shard = rc.run_shard
                WHERE rc.status = 'pending'
                  AND rc.dispatched_at IS NULL
                  AND NOT EXISTS (
                      SELECT 1
                      FROM selected_chunks
                      WHERE selected_chunks.run_id = rc.run_id
                        AND selected_chunks.run_shard = rc.run_shard
                        AND selected_chunks.id = rc.id
                  )
            ) AS has_remaining
        )
        SELECT
            claimed.id,
            claimed.run_key,
            claimed.run_shard,
            (SELECT COUNT(*)::bigint FROM chunk_events) AS chunk_events_enqueued,
            (SELECT COUNT(*)::bigint FROM marked_chunks) AS chunks_marked_dispatched,
            $21::bigint AS run_started_events_enqueued,
            remaining_chunks.has_remaining AS has_remaining_chunks
        FROM claimed, remaining_chunks
        "#,
    )
    .bind(chunk_window_size)
    .bind(route.run_id)
    .bind(route.run_shard)
    .bind(&snapshot.run_key)
    .bind(snapshot.dataset_id)
    .bind(snapshot.dataset_version_id)
    .bind(&snapshot.dataset_version)
    .bind(&snapshot.evaluation_profile_id)
    .bind(&snapshot.evaluation_profile_version)
    .bind(&snapshot.profile_version_id)
    .bind(&snapshot.profile_hash)
    .bind(&snapshot.aggregation_policy_id)
    .bind(&snapshot.aggregation_policy_version)
    .bind(&snapshot.aggregation_policy_hash)
    .bind(&snapshot.agent_provider)
    .bind(&snapshot.agent_name)
    .bind(&snapshot.agent_version)
    .bind(&snapshot.prompt_config_id)
    .bind(&snapshot.prompt_config_version)
    .bind(&snapshot.config_snapshot)
    .bind(snapshot.run_started_events_enqueued)
    .fetch_optional(execution_db)
    .await?;

    if let Some(dispatched) = &dispatched {
        let next_status = if dispatched.has_remaining_chunks {
            "open"
        } else {
            "drained"
        };
        sqlx::query(
            r#"
            UPDATE run_shard_dispatch_cursors
            SET status = $3,
                updated_at = now()
            WHERE run_id = $1::uuid
              AND run_shard = $2
            "#,
        )
        .bind(dispatched.id)
        .bind(dispatched.run_shard)
        .bind(next_status)
        .execute(&mut *control_tx)
        .await?;
        control_tx.commit().await?;

        run_shard_summary::refresh_run_shard_summary(
            execution_db,
            dispatched.id,
            dispatched.run_shard,
        )
        .await?;
    } else {
        control_tx.rollback().await?;
    }

    Ok(dispatched.map(|dispatched| DispatchedRun {
        id: dispatched.id,
        run_key: dispatched.run_key,
        run_shard: dispatched.run_shard,
        chunk_events_enqueued: dispatched.chunk_events_enqueued,
        chunks_marked_dispatched: dispatched.chunks_marked_dispatched,
        run_started_events_enqueued: dispatched.run_started_events_enqueued,
    }))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        time::Duration,
    };

    use sqlx::PgPool;

    use super::*;
    use crate::db::workflows::{
        run_cancel,
        run_finalize,
    };

    // --- Fixtures and assertions ---

    async fn seed_pending_run(pool: &PgPool, shard_chunk_counts: &[(i16, i32)]) -> Uuid {
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        let run_id = Uuid::now_v7();
        let expected_execution_count = shard_chunk_counts
            .iter()
            .map(|(_, count)| *count)
            .sum::<i32>();

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
                $5
            )
            "#,
        )
        .bind(run_id)
        .bind(format!("run-{run_id}"))
        .bind(dataset_id)
        .bind(dataset_version_id)
        .bind(expected_execution_count)
        .execute(pool)
        .await
        .unwrap();

        let mut ordinal = 0;
        for (run_shard, chunk_count) in shard_chunk_counts {
            sqlx::query(
                r#"
                INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
                VALUES ($1::uuid, $2, 'primary', 'active')
                ON CONFLICT (run_id, run_shard) DO UPDATE
                SET database_alias = EXCLUDED.database_alias,
                    status = EXCLUDED.status,
                    updated_at = now()
                "#,
            )
            .bind(run_id)
            .bind(run_shard)
            .execute(pool)
            .await
            .unwrap();

            sqlx::query(
                r#"
                INSERT INTO run_shard_dispatch_cursors (run_id, run_shard, status)
                VALUES ($1::uuid, $2, 'open')
                "#,
            )
            .bind(run_id)
            .bind(run_shard)
            .execute(pool)
            .await
            .unwrap();

            for _ in 0..*chunk_count {
                sqlx::query(
                    r#"
                    INSERT INTO run_chunks (
                        id,
                        run_id,
                        run_shard,
                        dataset_version_id,
                        profile_group_id,
                        ordinal_start,
                        ordinal_end,
                        status
                    )
                    VALUES (
                        $1::uuid,
                        $2::uuid,
                        $3,
                        $4::uuid,
                        'default',
                        $5,
                        $6,
                        'pending'
                    )
                    "#,
                )
                .bind(Uuid::now_v7())
                .bind(run_id)
                .bind(run_shard)
                .bind(dataset_version_id)
                .bind(ordinal)
                .bind(ordinal + 1)
                .execute(pool)
                .await
                .unwrap();

                ordinal += 1;
            }
        }

        run_id
    }

    async fn mark_run_running(pool: &PgPool, run_id: Uuid) {
        sqlx::query(
            r#"
            UPDATE runs
            SET status = 'running'::run_status,
                started_at = now(),
                dispatched_at = now(),
                updated_at = now()
            WHERE id = $1::uuid
            "#,
        )
        .bind(run_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn mark_all_chunks_completed(pool: &PgPool, run_id: Uuid) {
        sqlx::query(
            r#"
            UPDATE run_chunks
            SET status = 'completed',
                dispatched_at = COALESCE(dispatched_at, now()),
                updated_at = now()
            WHERE run_id = $1::uuid
            "#,
        )
        .bind(run_id)
        .execute(pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            UPDATE run_shard_dispatch_cursors
            SET status = 'drained',
                updated_at = now()
            WHERE run_id = $1::uuid
            "#,
        )
        .bind(run_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_shard_placements(pool: &PgPool, run_id: Uuid, placements: &[(i16, &str)]) {
        for (run_shard, status) in placements {
            sqlx::query(
                r#"
                INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
                VALUES ($1::uuid, $2, 'primary', $3)
                ON CONFLICT (run_id, run_shard) DO UPDATE
                SET database_alias = EXCLUDED.database_alias,
                    status = EXCLUDED.status,
                    updated_at = now()
                "#,
            )
            .bind(run_id)
            .bind(run_shard)
            .bind(status)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    async fn lock_run_for_share(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, run_id: Uuid) {
        sqlx::query(
            r#"
            SELECT id
            FROM runs
            WHERE id = $1::uuid
            FOR SHARE
            "#,
        )
        .bind(run_id)
        .execute(&mut **tx)
        .await
        .unwrap();
    }

    async fn lock_run_for_update(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, run_id: Uuid) {
        sqlx::query(
            r#"
            SELECT id
            FROM runs
            WHERE id = $1::uuid
            FOR UPDATE
            "#,
        )
        .bind(run_id)
        .execute(&mut **tx)
        .await
        .unwrap();
    }

    async fn dispatch_window(pool: &PgPool) -> Option<DispatchedRun> {
        dispatch_next_run_window(pool, Uuid::now_v7(), 60, 10)
            .await
            .unwrap()
    }

    async fn dispatched_chunk_count(pool: &PgPool, run_id: Uuid, run_shard: i16) -> i64 {
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)::bigint
            FROM run_chunks
            WHERE run_id = $1::uuid
              AND run_shard = $2
              AND dispatched_at IS NOT NULL
            "#,
        )
        .bind(run_id)
        .bind(run_shard)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn cursor_status(pool: &PgPool, run_id: Uuid, run_shard: i16) -> String {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT status
            FROM run_shard_dispatch_cursors
            WHERE run_id = $1::uuid
              AND run_shard = $2
            "#,
        )
        .bind(run_id)
        .bind(run_shard)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn cursor_status_count(pool: &PgPool, run_id: Uuid, status: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)::bigint
            FROM run_shard_dispatch_cursors
            WHERE run_id = $1::uuid
              AND status = $2
            "#,
        )
        .bind(run_id)
        .bind(status)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn run_status(pool: &PgPool, run_id: Uuid) -> String {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT status::text
            FROM runs
            WHERE id = $1::uuid
            "#,
        )
        .bind(run_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn chunk_ready_event_count(pool: &PgPool, run_id: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)::bigint
            FROM outbox_events
            WHERE aggregate_id = $1::uuid
              AND event_type = 'run.chunk.ready'
            "#,
        )
        .bind(run_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn run_started_event_count(pool: &PgPool, run_id: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)::bigint
            FROM outbox_events
            WHERE aggregate_id = $1::uuid
              AND event_type = 'run.started'
            "#,
        )
        .bind(run_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn run_snapshot_count(pool: &PgPool, run_id: Uuid, run_shard: i16) -> i64 {
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)::bigint
            FROM run_snapshots
            WHERE run_id = $1::uuid
              AND run_shard = $2
            "#,
        )
        .bind(run_id)
        .bind(run_shard)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn dispatched_shards(windows: &[DispatchedRun]) -> BTreeSet<i16> {
        windows.iter().map(|window| window.run_shard).collect()
    }

    // --- Shard cursor dispatch behavior ---

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn dispatch_scans_one_run_shard_at_a_time(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 2), (1, 2)]).await;

        let first = dispatch_window(&pool).await.unwrap();
        assert_eq!(first.id, run_id);
        assert_eq!(first.run_shard, 0);
        assert_eq!(first.chunks_marked_dispatched, 2);

        assert_eq!(dispatched_chunk_count(&pool, run_id, 1).await, 0);
        assert_eq!(cursor_status(&pool, run_id, 0).await, "drained");
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn select_next_dispatch_route_skips_moving_shard_placement(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 1), (1, 1)]).await;
        insert_shard_placements(&pool, run_id, &[(0, "moving"), (1, "active")]).await;

        let route = select_next_dispatch_route(&pool).await.unwrap().unwrap();

        assert_eq!(route.run_id, run_id);
        assert_eq!(route.run_shard, 1);
        assert_eq!(route.database_alias, "primary");
        assert_eq!(route.placement_status, "active");
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn count_dispatch_cursor_backlog_matches_dispatchable_routes(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 1), (1, 1), (2, 1)]).await;
        insert_shard_placements(
            &pool,
            run_id,
            &[(0, "active"), (1, "moving"), (2, "draining")],
        )
        .await;

        let backlog = count_dispatch_cursor_backlog(&pool).await.unwrap();

        assert_eq!(backlog, 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn dispatch_routed_run_window_dispatches_exact_route(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 1), (1, 1)]).await;
        let route = DispatchRoute {
            run_id,
            run_shard: 1,
            database_alias: "primary".to_string(),
            placement_status: "active".to_string(),
        };
        let snapshot = prepare_dispatch_run_snapshot(&pool, &route, Uuid::now_v7(), 60)
            .await
            .unwrap()
            .unwrap();

        let dispatched = dispatch_routed_run_window(&pool, &pool, 10, &route, &snapshot)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(dispatched.id, run_id);
        assert_eq!(dispatched.run_shard, 1);
        assert_eq!(dispatched_chunk_count(&pool, run_id, 0).await, 0);
        assert_eq!(dispatched_chunk_count(&pool, run_id, 1).await, 1);
        assert_eq!(run_snapshot_count(&pool, run_id, 1).await, 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn dispatch_releases_open_cursor_when_shard_has_more_chunks(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 3)]).await;

        let first = dispatch_next_run_window(&pool, Uuid::now_v7(), 60, 2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.run_shard, 0);
        assert_eq!(first.chunks_marked_dispatched, 2);
        assert_eq!(cursor_status(&pool, run_id, 0).await, "open");

        let second = dispatch_next_run_window(&pool, Uuid::now_v7(), 60, 2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.run_shard, 0);
        assert_eq!(second.chunks_marked_dispatched, 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn concurrent_dispatchers_claim_distinct_shards_for_same_running_run(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 1), (1, 1)]).await;
        mark_run_running(&pool, run_id).await;

        let (left, right) = tokio::join!(dispatch_window(&pool), dispatch_window(&pool));
        let windows = [left, right]
            .into_iter()
            .flatten()
            .collect::<Vec<DispatchedRun>>();

        assert_eq!(windows.len(), 2);
        assert_eq!(dispatched_shards(&windows), BTreeSet::from([0, 1]));
        assert_eq!(chunk_ready_event_count(&pool, run_id).await, 2);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn concurrent_dispatchers_do_not_duplicate_one_shard_cursor(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 1)]).await;
        mark_run_running(&pool, run_id).await;

        let (left, right) = tokio::join!(dispatch_window(&pool), dispatch_window(&pool));
        let windows = [left, right]
            .into_iter()
            .flatten()
            .collect::<Vec<DispatchedRun>>();

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].run_shard, 0);
        assert_eq!(dispatched_chunk_count(&pool, run_id, 0).await, 1);
        assert_eq!(chunk_ready_event_count(&pool, run_id).await, 1);
        assert_eq!(cursor_status(&pool, run_id, 0).await, "drained");
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn concurrent_pending_run_start_emits_one_started_event(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 1), (1, 1)]).await;

        let (left, right) = tokio::join!(dispatch_window(&pool), dispatch_window(&pool));
        let windows = [left, right]
            .into_iter()
            .flatten()
            .collect::<Vec<DispatchedRun>>();

        assert_eq!(windows.len(), 2);
        assert_eq!(dispatched_shards(&windows), BTreeSet::from([0, 1]));
        assert_eq!(run_status(&pool, run_id).await, "running");
        assert_eq!(run_started_event_count(&pool, run_id).await, 1);
        assert_eq!(chunk_ready_event_count(&pool, run_id).await, 2);
    }

    // --- Parent run lifecycle locking ---

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn running_dispatch_does_not_wait_on_parent_run_share_lock(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 1)]).await;
        mark_run_running(&pool, run_id).await;

        let mut tx = pool.begin().await.unwrap();
        lock_run_for_share(&mut tx, run_id).await;

        let dispatched = tokio::time::timeout(Duration::from_secs(1), dispatch_window(&pool))
            .await
            .expect("dispatch should not wait on a compatible parent run share lock")
            .unwrap();

        assert_eq!(dispatched.id, run_id);
        assert_eq!(dispatched.run_shard, 0);
        assert_eq!(dispatched.chunks_marked_dispatched, 1);
        assert_eq!(dispatched.run_started_events_enqueued, 0);

        tx.rollback().await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn running_dispatch_waits_on_parent_run_update_lock(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 1)]).await;
        mark_run_running(&pool, run_id).await;

        let mut tx = pool.begin().await.unwrap();
        lock_run_for_update(&mut tx, run_id).await;

        let dispatch_result =
            tokio::time::timeout(Duration::from_millis(100), dispatch_window(&pool)).await;
        assert!(
            dispatch_result.is_err(),
            "dispatch should wait behind an exclusive lifecycle update lock"
        );

        tx.rollback().await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn cancel_waits_behind_active_dispatch_lifecycle_share_lock(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 1)]).await;
        mark_run_running(&pool, run_id).await;

        let mut tx = pool.begin().await.unwrap();
        lock_run_for_share(&mut tx, run_id).await;

        let cancel_result = tokio::time::timeout(
            Duration::from_millis(100),
            run_cancel::cancel_run(&pool, run_id),
        )
        .await;
        assert!(
            cancel_result.is_err(),
            "cancellation should wait behind active dispatch lifecycle locks"
        );

        tx.rollback().await.unwrap();

        let outcome = run_cancel::cancel_run(&pool, run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(outcome.cancelled);
        assert_eq!(run_status(&pool, run_id).await, "cancelled");
        assert_eq!(cursor_status_count(&pool, run_id, "drained").await, 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
    async fn finalization_skips_run_with_active_dispatch_lifecycle_share_lock(pool: PgPool) {
        let run_id = seed_pending_run(&pool, &[(0, 1)]).await;
        mark_run_running(&pool, run_id).await;
        mark_all_chunks_completed(&pool, run_id).await;

        let mut tx = pool.begin().await.unwrap();
        lock_run_for_share(&mut tx, run_id).await;

        let skipped = run_finalize::claim_next_finalizable_run(&pool, Uuid::now_v7(), 60)
            .await
            .unwrap();
        assert!(skipped.is_none());

        tx.rollback().await.unwrap();

        let claimed = run_finalize::claim_next_finalizable_run(&pool, Uuid::now_v7(), 60)
            .await
            .unwrap();
        assert_eq!(claimed.unwrap().id, run_id);
    }
}
