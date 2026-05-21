//! Outbox event table access.
//!
//! The outbox table is used for durable, idempotent event publication. Claiming
//! publishable events uses `FOR UPDATE SKIP LOCKED` so multiple publishers can
//! work concurrently without taking the same event.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::outbox_event::{
    OutboxEvent,
    OutboxEventDraft,
    OutboxEventPatch,
};

/// Inserts one outbox event.
pub(crate) async fn insert_outbox_event(
    db: &PgPool,
    draft: &OutboxEventDraft,
) -> anyhow::Result<OutboxEvent> {
    let event = sqlx::query_as::<_, OutboxEvent>(
        r#"
        INSERT INTO outbox_events (
            event_type, aggregate_type, aggregate_id, dedupe_key
        )
        VALUES ($1, $2, $3::uuid, $4)
        RETURNING
            id,
            event_type,
            aggregate_type,
            aggregate_id,
            dedupe_key,
            payload,
            status::text as status,
            available_at,
            claim_token,
            claimed_until,
            publish_attempt_count,
            published_at,
            error_message,
            created_at,
            updated_at
        "#,
    )
    .bind(&draft.event_type)
    .bind(&draft.aggregate_type)
    .bind(draft.aggregate_id)
    .bind(&draft.dedupe_key)
    .fetch_one(db)
    .await?;

    Ok(event)
}

/// Finds an outbox event by primary key.
pub(crate) async fn select_outbox_event_by_id(
    db: &PgPool,
    id: Uuid,
) -> anyhow::Result<Option<OutboxEvent>> {
    let event = sqlx::query_as::<_, OutboxEvent>(
        r#"
        SELECT
            id,
            event_type,
            aggregate_type,
            aggregate_id,
            dedupe_key,
            payload,
            status::text as status,
            available_at,
            claim_token,
            claimed_until,
            publish_attempt_count,
            published_at,
            error_message,
            created_at,
            updated_at
        FROM outbox_events
        WHERE id = $1::uuid
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(event)
}

/// Lists outbox events with a given status.
pub(crate) async fn list_outbox_events_by_status(
    db: &PgPool,
    status: &str,
    limit: i64,
) -> anyhow::Result<Vec<OutboxEvent>> {
    let events = sqlx::query_as::<_, OutboxEvent>(
        r#"
        SELECT
            id,
            event_type,
            aggregate_type,
            aggregate_id,
            dedupe_key,
            payload,
            status::text as status,
            available_at,
            claim_token,
            claimed_until,
            publish_attempt_count,
            published_at,
            error_message,
            created_at,
            updated_at
        FROM outbox_events
        WHERE status = $1::outbox_status
        ORDER BY available_at ASC
        LIMIT $2
        "#,
    )
    .bind(status)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(events)
}

/// Claims pending outbox events by pushing their availability into the future.
///
/// The returned rows are the caller's publish lease. If publishing fails, use
/// `reschedule_outbox_event` to make the event available for retry.
pub(crate) async fn claim_publishable_outbox_events(
    db: &PgPool,
    limit: i64,
    lease_seconds: i32,
) -> anyhow::Result<Vec<OutboxEvent>> {
    let events = sqlx::query_as::<_, OutboxEvent>(
        r#"
        WITH claim AS (
            SELECT id
            FROM outbox_events
            WHERE status = 'pending'::outbox_status
              AND available_at <= now()
            ORDER BY available_at ASC
            FOR UPDATE SKIP LOCKED
            LIMIT $1
        )
        UPDATE outbox_events oe
        SET claim_token = gen_random_uuid(),
            claimed_until = now() + ($2::int * interval '1 second'),
            available_at = now() + ($2::int * interval '1 second'),
            publish_attempt_count = publish_attempt_count + 1,
            updated_at = now()
        FROM claim
        WHERE oe.id = claim.id
        RETURNING
            oe.id,
            oe.event_type,
            oe.aggregate_type,
            oe.aggregate_id,
            oe.dedupe_key,
            oe.payload,
            oe.status::text as status,
            oe.available_at,
            oe.claim_token,
            oe.claimed_until,
            oe.publish_attempt_count,
            oe.published_at,
            oe.error_message,
            oe.created_at,
            oe.updated_at
        "#,
    )
    .bind(limit)
    .bind(lease_seconds)
    .fetch_all(db)
    .await?;

    Ok(events)
}

/// Marks an outbox event as successfully published.
pub(crate) async fn mark_outbox_event_published(
    db: &PgPool,
    id: Uuid,
    claim_token: Uuid,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        UPDATE outbox_events
        SET status = 'published'::outbox_status,
            published_at = now(),
            claim_token = NULL,
            claimed_until = NULL,
            error_message = NULL,
            updated_at = now()
        WHERE id = $1::uuid
          AND claim_token = $2::uuid
          AND status = 'pending'::outbox_status
        "#,
    )
    .bind(id)
    .bind(claim_token)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

/// Reschedules a failed publish attempt for later retry.
pub(crate) async fn reschedule_outbox_event(
    db: &PgPool,
    id: Uuid,
    claim_token: Uuid,
    retry_after_seconds: i32,
    error_message: &str,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        UPDATE outbox_events
        SET status = 'pending'::outbox_status,
            available_at = now() + ($3::int * interval '1 second'),
            claim_token = NULL,
            claimed_until = NULL,
            error_message = $4,
            updated_at = now()
        WHERE id = $1::uuid
          AND claim_token = $2::uuid
          AND status = 'pending'::outbox_status
        "#,
    )
    .bind(id)
    .bind(claim_token)
    .bind(retry_after_seconds)
    .bind(error_message)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

/// Updates outbox status fields directly.
pub(crate) async fn update_outbox_event_status(
    db: &PgPool,
    id: Uuid,
    patch: &OutboxEventPatch,
) -> anyhow::Result<Option<OutboxEvent>> {
    let event = sqlx::query_as::<_, OutboxEvent>(
        r#"
        UPDATE outbox_events
        SET status = $2::outbox_status,
            published_at = CASE WHEN $2 = 'published' THEN now() ELSE published_at END,
            claim_token = NULL,
            claimed_until = NULL,
            error_message = $3,
            updated_at = now()
        WHERE id = $1::uuid
        RETURNING
            id,
            event_type,
            aggregate_type,
            aggregate_id,
            dedupe_key,
            payload,
            status::text as status,
            available_at,
            claim_token,
            claimed_until,
            publish_attempt_count,
            published_at,
            error_message,
            created_at,
            updated_at
        "#,
    )
    .bind(id)
    .bind(&patch.status)
    .bind(&patch.error_message)
    .fetch_optional(db)
    .await?;

    Ok(event)
}

/// Deletes an outbox event by primary key.
pub(crate) async fn delete_outbox_event_by_id(db: &PgPool, id: Uuid) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        DELETE FROM outbox_events
        WHERE id = $1::uuid
        "#,
    )
    .bind(id)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}
