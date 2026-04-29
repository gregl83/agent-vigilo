use sqlx::PgPool;
use uuid::Uuid;

use crate::models::evaluator::{
    Evaluator,
    EvaluatorDraft,
    EvaluatorPatch,
};

pub(crate) async fn insert_evaluator(db: &PgPool, draft: &EvaluatorDraft) -> anyhow::Result<Evaluator> {
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
            description, tags, metadata, is_active, created_at, updated_at
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

pub(crate) async fn select_evaluator_by_id(db: &PgPool, id: Uuid) -> anyhow::Result<Option<Evaluator>> {
    let evaluator = sqlx::query_as::<_, Evaluator>(
        r#"
        SELECT
            id, namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, runtime_fingerprint,
            description, tags, metadata, is_active, created_at, updated_at
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
            description, tags, metadata, is_active, created_at, updated_at
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

pub(crate) async fn list_evaluators(db: &PgPool, namespace: &str) -> anyhow::Result<Vec<Evaluator>> {
    let evaluators = sqlx::query_as::<_, Evaluator>(
        r#"
        SELECT
            id, namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, runtime_fingerprint,
            description, tags, metadata, is_active, created_at, updated_at
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

pub(crate) async fn update_evaluator_active_by_name(
    db: &PgPool,
    namespace: &str,
    name: &str,
    patch: &EvaluatorPatch,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        UPDATE evaluators
        SET is_active = $3,
            updated_at = now()
        WHERE namespace = $1 AND name = $2
        "#,
    )
    .bind(namespace)
    .bind(name)
    .bind(patch.is_active)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

pub(crate) async fn delete_evaluator_by_name(
    db: &PgPool,
    namespace: &str,
    name: &str,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        DELETE FROM evaluators
        WHERE namespace = $1 AND name = $2
        "#,
    )
    .bind(namespace)
    .bind(name)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}
