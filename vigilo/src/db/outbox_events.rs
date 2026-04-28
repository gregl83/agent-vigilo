use sqlx::PgPool;

use crate::models::outbox_event::{
    OutboxEvent,
    OutboxEventDraft,
    OutboxEventPatch,
};

const SELECT_COLUMNS: &str = r#"
    id::text AS id,
    event_type,
    aggregate_type,
    aggregate_id::text AS aggregate_id,
    dedupe_key,
    payload,
    status::text AS status,
    available_at::text AS available_at,
    published_at::text AS published_at,
    error_message,
    created_at::text AS created_at,
    updated_at::text AS updated_at
"#;


pub(crate) async fn insert_outbox_event(db: &PgPool, draft: &OutboxEventDraft) -> anyhow::Result<OutboxEvent> {
    let event = sqlx::query_as::<_, OutboxEvent>(&format!(
        r#"
        INSERT INTO outbox_events (
            event_type,
            aggregate_type,
            aggregate_id,
            dedupe_key
        )
        VALUES ($1, $2, $3::uuid, $4)
        RETURNING {}
        "#,
        SELECT_COLUMNS
    ))
    .bind(&draft.event_type)
    .bind(&draft.aggregate_type)
    .bind(&draft.aggregate_id)
    .bind(&draft.dedupe_key)
    .fetch_one(db)
    .await?;

    Ok(event)
}

pub(crate) async fn select_outbox_event_by_id(db: &PgPool, id: &str) -> anyhow::Result<Option<OutboxEvent>> {
    let event = sqlx::query_as::<_, OutboxEvent>(&format!(
        r#"
        SELECT {}
        FROM outbox_events
        WHERE id = $1::uuid
        "#,
        SELECT_COLUMNS
    ))
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
    let events = sqlx::query_as::<_, OutboxEvent>(&format!(
        r#"
        SELECT {}
        FROM outbox_events
        WHERE status = $1::outbox_status
        ORDER BY available_at ASC
        LIMIT $2
        "#,
        SELECT_COLUMNS
    ))
    .bind(status)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(events)
}

pub(crate) async fn update_outbox_event_status(
    db: &PgPool,
    id: &str,
    patch: &OutboxEventPatch,
) -> anyhow::Result<Option<OutboxEvent>> {
    let event = sqlx::query_as::<_, OutboxEvent>(&format!(
        r#"
        UPDATE outbox_events
        SET status = $2::outbox_status,
            published_at = CASE WHEN $2 = 'published' THEN now() ELSE published_at END,
            error_message = $3,
            updated_at = now()
        WHERE id = $1::uuid
        RETURNING {}
        "#,
        SELECT_COLUMNS
    ))
    .bind(id)
    .bind(&patch.status)
    .bind(&patch.error_message)
    .fetch_optional(db)
    .await?;

    Ok(event)
}

pub(crate) async fn delete_outbox_event_by_id(db: &PgPool, id: &str) -> anyhow::Result<u64> {
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

