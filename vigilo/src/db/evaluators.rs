use sqlx::PgPool;
use uuid::Uuid;

use crate::models::evaluator::{
    Evaluator,
    EvaluatorDraft,
    EvaluatorPatch,
};

pub(crate) async fn insert_evaluator(db: &PgPool, draft: &EvaluatorDraft) -> anyhow::Result<Evaluator> {
    // sqlx handles the conversion of usize to i64 for Postgres BIGINT
    let wasm_size_bytes = draft.wasm_bytes.len() as i64;

    let evaluator = sqlx::query_as!(
        Evaluator,
        r#"
        INSERT INTO evaluators (
            namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, description
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        RETURNING
            id, namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, description,
            tags, metadata, is_active, created_at, updated_at
        "#,
        draft.namespace,
        draft.name,
        draft.version,
        draft.content_hash,
        draft.wasm_bytes,
        wasm_size_bytes,
        draft.interface_name,
        draft.interface_version,
        draft.wit_world,
        draft.runtime,
        draft.runtime_version,
        draft.description
    )
        .fetch_one(db)
        .await?;

    Ok(evaluator)
}

pub(crate) async fn select_evaluator_by_id(db: &PgPool, id: Uuid) -> anyhow::Result<Option<Evaluator>> {
    let evaluator = sqlx::query_as!(
        Evaluator,
        r#"
        SELECT
            id, namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, description,
            tags, metadata, is_active, created_at, updated_at
        FROM evaluators
        WHERE id = $1
        "#,
        id
    )
        .fetch_optional(db)
        .await?;

    Ok(evaluator)
}

pub(crate) async fn select_latest_evaluator_by_name(
    db: &PgPool,
    namespace: &str,
    name: &str,
) -> anyhow::Result<Option<Evaluator>> {
    let evaluator = sqlx::query_as!(
        Evaluator,
        r#"
        SELECT
            id, namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, description,
            tags, metadata, is_active, created_at, updated_at
        FROM evaluators
        WHERE namespace = $1 AND name = $2
        ORDER BY created_at DESC
        LIMIT 1
        "#,
        namespace,
        name
    )
        .fetch_optional(db)
        .await?;

    Ok(evaluator)
}

pub(crate) async fn list_evaluators(db: &PgPool, namespace: &str) -> anyhow::Result<Vec<Evaluator>> {
    let evaluators = sqlx::query_as!(
        Evaluator,
        r#"
        SELECT
            id, namespace, name, version, content_hash, wasm_bytes,
            wasm_size_bytes, interface_name, interface_version,
            wit_world, runtime, runtime_version, description,
            tags, metadata, is_active, created_at, updated_at
        FROM evaluators
        WHERE namespace = $1
        ORDER BY name ASC, version DESC
        "#,
        namespace
    )
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
    let result = sqlx::query!(
        r#"
        UPDATE evaluators
        SET is_active = $3,
            updated_at = now()
        WHERE namespace = $1 AND name = $2
        "#,
        namespace,
        name,
        patch.is_active
    )
        .execute(db)
        .await?;

    Ok(result.rows_affected())
}

pub(crate) async fn delete_evaluator_by_name(
    db: &PgPool,
    namespace: &str,
    name: &str,
) -> anyhow::Result<u64> {
    let result = sqlx::query!(
        r#"
        DELETE FROM evaluators
        WHERE namespace = $1 AND name = $2
        "#,
        namespace,
        name
    )
        .execute(db)
        .await?;

    Ok(result.rows_affected())
}
