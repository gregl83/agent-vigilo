//! Run dispatch workflow helpers.
//!
//! Coordinators use this module to atomically start pending runs and enqueue
//! bounded windows of outbox events that make chunks visible to workers. Run
//! and chunk row locks keep multiple coordinators from dispatching the same
//! window concurrently.

use sqlx::PgPool;
use uuid::Uuid;

/// Run projection returned after a coordinator dispatches a pending run.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct DispatchedRun {
    pub(crate) id: Uuid,
    pub(crate) run_key: String,
    pub(crate) chunk_events_enqueued: i64,
    pub(crate) chunks_marked_dispatched: i64,
    pub(crate) run_started_events_enqueued: i64,
}

/// Claims one dispatchable run and enqueues a bounded chunk-ready window.
///
/// Pending runs are first marked running and receive a `run.started` event.
/// Running runs are eligible while they still have pending chunks whose
/// `dispatched_at` cursor is open. Chunk-ready events and cursor updates happen
/// in the same statement so a rollback does not lose undispatched work.
pub(crate) async fn dispatch_next_run_window(
    db: &PgPool,
    coordinator_id: Uuid,
    lease_seconds: i32,
    chunk_window_size: i64,
) -> anyhow::Result<Option<DispatchedRun>> {
    let dispatched = sqlx::query_as::<_, DispatchedRun>(
        r#"
        WITH candidate AS (
            SELECT r.id, r.status AS previous_status
            FROM runs r
            WHERE (
                    r.status = 'pending'::run_status
                    AND (
                        r.coordinator_leased_until IS NULL
                        OR r.coordinator_leased_until < now()
                    )
                )
               OR (
                    r.status = 'running'::run_status
                    AND EXISTS (
                        SELECT 1
                        FROM run_chunks rc
                        WHERE rc.run_id = r.id
                          AND rc.status = 'pending'
                          AND rc.dispatched_at IS NULL
                    )
                )
            ORDER BY r.updated_at ASC, r.created_at ASC
            FOR UPDATE SKIP LOCKED
            LIMIT 1
        ),
        claimed AS (
            UPDATE runs AS r
            SET status = 'running'::run_status,
                coordinator_id = $1,
                coordinator_leased_until = CASE
                    WHEN candidate.previous_status = 'pending'::run_status
                    THEN now() + ($2::int * interval '1 second')
                    ELSE r.coordinator_leased_until
                END,
                coordinator_heartbeat_at = now(),
                started_at = COALESCE(r.started_at, now()),
                dispatched_at = COALESCE(r.dispatched_at, now()),
                updated_at = now()
            FROM candidate
            WHERE r.id = candidate.id
            RETURNING r.id, r.run_key, candidate.previous_status
        ),
        selected_chunks AS (
            SELECT rc.run_id, rc.id
            FROM run_chunks rc
            JOIN claimed
              ON claimed.id = rc.run_id
            WHERE rc.status = 'pending'
              AND rc.dispatched_at IS NULL
            ORDER BY rc.ordinal_start ASC, rc.id ASC
            LIMIT $3
        ),
        chunk_events AS (
            INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload)
            SELECT
                'run.chunk.ready',
                'run',
                selected_chunks.run_id,
                format('run:%s:chunk:%s:ready', selected_chunks.run_id, selected_chunks.id),
                jsonb_build_object('run_id', selected_chunks.run_id, 'chunk_id', selected_chunks.id)
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
              AND rc.id = selected_chunks.id
            RETURNING rc.id
        ),
        started_event AS (
            INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload)
            SELECT
                'run.started',
                'run',
                claimed.id,
                format('run:%s:started', claimed.id),
                jsonb_build_object('run_id', claimed.id)
            FROM claimed
            WHERE claimed.previous_status = 'pending'::run_status
            ON CONFLICT (dedupe_key) DO NOTHING
            RETURNING id
        )
        SELECT
            claimed.id,
            claimed.run_key,
            (SELECT COUNT(*)::bigint FROM chunk_events) AS chunk_events_enqueued,
            (SELECT COUNT(*)::bigint FROM marked_chunks) AS chunks_marked_dispatched,
            (SELECT COUNT(*)::bigint FROM started_event) AS run_started_events_enqueued
        FROM claimed
        "#,
    )
    .bind(coordinator_id)
    .bind(lease_seconds)
    .bind(chunk_window_size)
    .fetch_optional(db)
    .await?;

    Ok(dispatched)
}
