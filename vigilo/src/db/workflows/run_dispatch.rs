//! Run dispatch workflow helpers.
//!
//! Coordinators use this module to atomically claim pending runs, mark them
//! running, and enqueue the outbox events that make chunks visible to workers.
//! The claim query uses row locking so multiple coordinators can safely dispatch
//! runs concurrently.

use sqlx::PgPool;
use uuid::Uuid;

/// Run projection returned after a coordinator dispatches a pending run.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct DispatchedRun {
    pub(crate) id: Uuid,
    pub(crate) run_key: String,
    pub(crate) chunk_events_enqueued: i64,
    pub(crate) run_started_events_enqueued: i64,
}

/// Claims and dispatches the oldest pending run whose coordinator lease is open.
///
/// Chunk-ready events are inserted in the same statement that marks the run
/// running. This prevents workers from claiming chunks before dispatch has
/// established run ownership.
pub(crate) async fn dispatch_next_pending_run(
    db: &PgPool,
    coordinator_id: Uuid,
    lease_seconds: i32,
) -> anyhow::Result<Option<DispatchedRun>> {
    let dispatched = sqlx::query_as::<_, DispatchedRun>(
        r#"
        WITH candidate AS (
            SELECT id
            FROM runs
            WHERE status = 'pending'::run_status
              AND (coordinator_leased_until IS NULL OR coordinator_leased_until < now())
            ORDER BY created_at ASC
            FOR UPDATE SKIP LOCKED
            LIMIT 1
        ),
        claimed AS (
            UPDATE runs AS r
            SET status = 'running'::run_status,
                coordinator_id = $1,
                coordinator_leased_until = now() + ($2::int * interval '1 second'),
                coordinator_heartbeat_at = now(),
                started_at = COALESCE(r.started_at, now()),
                dispatched_at = COALESCE(r.dispatched_at, now()),
                updated_at = now()
            FROM candidate
            WHERE r.id = candidate.id
            RETURNING r.id, r.run_key
        ),
        chunk_events AS (
            INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload)
            SELECT
                'run.chunk.ready',
                'run',
                rc.run_id,
                format('run:%s:chunk:%s:ready', rc.run_id, rc.id),
                jsonb_build_object('run_id', rc.run_id, 'chunk_id', rc.id)
            FROM run_chunks rc
            JOIN claimed
              ON claimed.id = rc.run_id
            WHERE rc.status = 'pending'
            ON CONFLICT (dedupe_key) DO NOTHING
            RETURNING id
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
            ON CONFLICT (dedupe_key) DO NOTHING
            RETURNING id
        )
        SELECT
            claimed.id,
            claimed.run_key,
            (SELECT COUNT(*)::bigint FROM chunk_events) AS chunk_events_enqueued,
            (SELECT COUNT(*)::bigint FROM started_event) AS run_started_events_enqueued
        FROM claimed
        "#,
    )
    .bind(coordinator_id)
    .bind(lease_seconds)
    .fetch_optional(db)
    .await?;

    Ok(dispatched)
}
