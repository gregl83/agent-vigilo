//! PostgreSQL operations for case projection workflows.

use futures_util::TryStreamExt;
use sqlx::{
    PgPool,
    Postgres,
    QueryBuilder,
    Transaction,
};
use uuid::Uuid;

use super::update_projection_hash;
use crate::models::run_shard_case::RunShardCaseDraft;

/// Streams the persisted projection so verification memory stays bounded.
pub(crate) async fn projection_fingerprint(
    db: &PgPool,
    run_id: Uuid,
    run_shards: &[i16],
) -> anyhow::Result<(i64, String)> {
    let mut rows = sqlx::query_as::<_, RunShardCaseDraft>(
        r#"
        SELECT run_id, run_shard, dataset_version_id, case_id, case_ordinal, case_hash
        FROM run_shard_cases
        WHERE run_id = $1::uuid
          AND run_shard = ANY($2::smallint[])
        ORDER BY case_ordinal, case_id
        "#,
    )
    .bind(run_id)
    .bind(run_shards)
    .fetch(db);
    let mut count = 0;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"vigilo/run-shard-cases/v1");
    while let Some(row) = rows.try_next().await? {
        update_projection_hash(&mut hasher, &row);
        count += 1;
    }
    Ok((count, hasher.finalize().to_hex().to_string()))
}

/// Inserts an immutable projection page and rejects conflicting replays.
pub(crate) async fn insert_projection_page(
    tx: &mut Transaction<'_, Postgres>,
    rows: &[RunShardCaseDraft],
) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut query = QueryBuilder::<Postgres>::new(
        "INSERT INTO run_shard_cases \
         (run_id, run_shard, dataset_version_id, case_id, case_ordinal, case_hash) ",
    );
    query.push_values(rows, |mut row, projection| {
        row.push_bind(projection.run_id)
            .push_bind(projection.run_shard)
            .push_bind(projection.dataset_version_id)
            .push_bind(projection.case_id)
            .push_bind(projection.case_ordinal)
            .push_bind(&projection.case_hash);
    });
    query.push(" ON CONFLICT DO NOTHING");
    query.build().execute(tx.as_mut()).await?;

    let run_id = rows[0].run_id;
    let ordinals = rows.iter().map(|row| row.case_ordinal).collect::<Vec<_>>();
    let persisted = sqlx::query_as::<_, RunShardCaseDraft>(
        r#"
        SELECT run_id, run_shard, dataset_version_id, case_id, case_ordinal, case_hash
        FROM run_shard_cases
        WHERE run_id = $1::uuid
          AND case_ordinal = ANY($2::integer[])
        ORDER BY case_ordinal, case_id
        "#,
    )
    .bind(run_id)
    .bind(&ordinals)
    .fetch_all(tx.as_mut())
    .await?;
    if persisted != rows {
        anyhow::bail!(
            "run {} projection page conflicts with persisted immutable rows",
            run_id
        );
    }
    Ok(())
}
