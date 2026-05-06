use sqlx::PgPool;
use uuid::Uuid;

use crate::models::evaluator::{
    Evaluator,
    EvaluatorDraft,
    EvaluatorPatch,
    EvaluatorSummary,
};

pub(crate) async fn insert_evaluator(
    db: &PgPool,
    draft: &EvaluatorDraft,
) -> anyhow::Result<Evaluator> {
    let wasm_size_bytes = draft.wasm_bytes.len() as i64;

    let evaluator = sqlx::query_as::<_, Evaluator>(
        r#"
        INSERT INTO evaluators (
            namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, runtime_fingerprint,
            description, tags, metadata
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        RETURNING
            id, namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, runtime_fingerprint,
            description, tags, metadata, state, state_reason, created_at, updated_at
        "#,
    )
    .bind(&draft.namespace)
    .bind(&draft.name)
    .bind(&draft.version)
    .bind(&draft.content_hash)
    .bind(&draft.wasm_bytes)
    .bind(wasm_size_bytes)
    .bind(&draft.interface_name)
    .bind(&draft.interface_version)
    .bind(&draft.wit_world)
    .bind(&draft.runtime)
    .bind(&draft.runtime_version)
    .bind(&draft.runtime_fingerprint)
    .bind(&draft.description)
    .bind(&draft.tags)
    .bind(&draft.metadata)
    .fetch_one(db)
    .await?;

    Ok(evaluator)
}

pub(crate) async fn select_evaluator_by_id(
    db: &PgPool,
    id: Uuid,
) -> anyhow::Result<Option<Evaluator>> {
    let evaluator = sqlx::query_as::<_, Evaluator>(
        r#"
        SELECT
            id, namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, runtime_fingerprint,
            description, tags, metadata, state, state_reason, created_at, updated_at
        FROM evaluators
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(evaluator)
}

pub(crate) async fn select_latest_evaluator_by_name(
    db: &PgPool,
    namespace: &str,
    name: &str,
) -> anyhow::Result<Option<Evaluator>> {
    let evaluator = sqlx::query_as::<_, Evaluator>(
        r#"
        SELECT
            id, namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, runtime_fingerprint,
            description, tags, metadata, state, state_reason, created_at, updated_at
        FROM evaluators
        WHERE namespace = $1 AND name = $2
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(namespace)
    .bind(name)
    .fetch_optional(db)
    .await?;

    Ok(evaluator)
}

pub(crate) async fn select_evaluator(
    db: &PgPool,
    namespace: &str,
    name: &str,
    version: &str,
) -> anyhow::Result<Option<Evaluator>> {
    let evaluator = sqlx::query_as::<_, Evaluator>(
        r#"
        SELECT
            id, namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, runtime_fingerprint,
            description, tags, metadata, state, state_reason, created_at, updated_at
        FROM evaluators
        WHERE namespace = $1 AND name = $2 AND version = $3
        LIMIT 1
        "#,
    )
    .bind(namespace)
    .bind(name)
    .bind(version)
    .fetch_optional(db)
    .await?;

    Ok(evaluator)
}

pub(crate) async fn list_evaluators(
    db: &PgPool,
    namespace: &str,
) -> anyhow::Result<Vec<Evaluator>> {
    let evaluators = sqlx::query_as::<_, Evaluator>(
        r#"
        SELECT
            id, namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, runtime_fingerprint,
            description, tags, metadata, state, state_reason, created_at, updated_at
        FROM evaluators
        WHERE namespace = $1
        ORDER BY name ASC, version DESC
        "#,
    )
    .bind(namespace)
    .fetch_all(db)
    .await?;

    Ok(evaluators)
}

pub(crate) async fn search_evaluator_summaries(
    db: &PgPool,
    namespace: &str,
    query: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<EvaluatorSummary>> {
    let limit = limit.clamp(1, 20);

    let pattern = query
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("%{}%", value));

    let evaluators = sqlx::query_as::<_, EvaluatorSummary>(
        r#"
        SELECT
            namespace, name, version, description, tags, metadata, state, state_reason
        FROM evaluators
        WHERE namespace = $1
          AND (
              $2::text IS NULL
              OR name ILIKE $2
              OR COALESCE(description, '') ILIKE $2
              OR COALESCE(state_reason, '') ILIKE $2
              OR tags::text ILIKE $2
              OR metadata::text ILIKE $2
          )
        ORDER BY name ASC, version DESC
        LIMIT $3
        "#,
    )
    .bind(namespace)
    .bind(pattern)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(evaluators)
}

pub(crate) async fn update_evaluator_state(
    db: &PgPool,
    namespace: &str,
    name: &str,
    version: &str,
    patch: &EvaluatorPatch,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        UPDATE evaluators
        SET state = $3,
            state_reason = $4,
            updated_at = now()
        WHERE namespace = $1 AND name = $2 AND version = $5
          AND state <> 'removed'::evaluator_state
        "#,
    )
    .bind(namespace)
    .bind(name)
    .bind(&patch.state)
    .bind(&patch.state_reason)
    .bind(version)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}
