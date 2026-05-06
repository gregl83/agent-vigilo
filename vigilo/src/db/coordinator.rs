use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct ClaimedRun {
    pub(crate) id: Uuid,
    pub(crate) run_key: String,
}

pub(crate) async fn claim_next_pending_run(
    db: &PgPool,
    coordinator_id: &str,
    lease_seconds: i32,
) -> anyhow::Result<Option<ClaimedRun>> {
    let claimed = sqlx::query_as::<_, ClaimedRun>(
        r#"
        WITH candidate AS (
            SELECT id
            FROM runs
            WHERE status = 'pending'::run_status
              AND (coordinator_leased_until IS NULL OR coordinator_leased_until < now())
            ORDER BY created_at ASC
            FOR UPDATE SKIP LOCKED
            LIMIT 1
        )
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
        "#,
    )
    .bind(coordinator_id)
    .bind(lease_seconds)
    .fetch_optional(db)
    .await?;

    Ok(claimed)
}

pub(crate) async fn enqueue_missing_chunk_ready_events(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload)
        SELECT
            'run.chunk.ready',
            'run',
            rc.run_id,
            format('run:%s:chunk:%s:ready', rc.run_id, rc.id),
            jsonb_build_object('run_id', rc.run_id, 'chunk_id', rc.id)
        FROM run_chunks rc
        WHERE rc.run_id = $1::uuid
          AND rc.status = 'pending'
        ON CONFLICT (dedupe_key) DO NOTHING
        "#,
    )
    .bind(run_id)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

pub(crate) async fn enqueue_run_started_event(db: &PgPool, run_id: Uuid) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        INSERT INTO outbox_events (event_type, aggregate_type, aggregate_id, dedupe_key, payload)
        VALUES (
            'run.started',
            'run',
            $1::uuid,
            format('run:%s:started', $1::uuid),
            $2::jsonb
        )
        ON CONFLICT (dedupe_key) DO NOTHING
        "#,
    )
    .bind(run_id)
    .bind(json!({ "run_id": run_id }))
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}
