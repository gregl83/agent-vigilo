use sqlx::PgPool;
use uuid::Uuid;

use crate::models::outbox_event::{
    OutboxEvent,
    OutboxEventDraft,
    OutboxEventPatch,
};

pub(crate) async fn insert_outbox_event(db: &PgPool, draft: &OutboxEventDraft) -> anyhow::Result<OutboxEvent> {
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
