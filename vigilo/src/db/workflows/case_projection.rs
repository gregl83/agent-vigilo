//! Validation and persistence helpers for shard-local case projections.

use std::collections::{
    BTreeMap,
    BTreeSet,
};

use futures_util::TryStreamExt;
use sqlx::{
    PgPool,
    Postgres,
    QueryBuilder,
    Transaction,
};
use uuid::Uuid;

use crate::models::{
    dataset_version_case::DatasetVersionCaseDraft,
    run_chunk::RunChunkDraft,
    run_shard_case::RunShardCaseDraft,
};

pub(crate) fn project_cases_for_chunks(
    run_id: Uuid,
    dataset_version_id: Uuid,
    dataset_cases: &[DatasetVersionCaseDraft],
    chunks: &[RunChunkDraft],
) -> anyhow::Result<Vec<RunShardCaseDraft>> {
    let cases_by_ordinal = dataset_cases
        .iter()
        .map(|case| (case.case_ordinal, case))
        .collect::<BTreeMap<_, _>>();
    if cases_by_ordinal.len() != dataset_cases.len() {
        anyhow::bail!("canonical dataset contains duplicate case ordinals");
    }

    let mut claimed_ordinals = BTreeSet::new();
    let mut projection = Vec::new();
    for chunk in chunks {
        if chunk.ordinal_start < 0 || chunk.ordinal_end <= chunk.ordinal_start {
            anyhow::bail!(
                "chunk {} has invalid ordinal range [{}, {})",
                chunk.chunk_id,
                chunk.ordinal_start,
                chunk.ordinal_end
            );
        }
        for ordinal in chunk.ordinal_start..chunk.ordinal_end {
            if !claimed_ordinals.insert(ordinal) {
                anyhow::bail!("case ordinal {ordinal} is assigned to multiple chunks");
            }
            let case = cases_by_ordinal.get(&ordinal).ok_or_else(|| {
                anyhow::anyhow!(
                    "chunk {} references missing canonical case ordinal {ordinal}",
                    chunk.chunk_id
                )
            })?;
            projection.push(RunShardCaseDraft {
                run_id,
                run_shard: chunk.run_shard,
                dataset_version_id,
                case_id: case.case_id,
                case_ordinal: ordinal,
                case_hash: case.case_hash.clone(),
            });
        }
    }
    projection.sort_by_key(|row| (row.case_ordinal, row.case_id));
    Ok(projection)
}

/// Hashes immutable projection identity using a versioned binary encoding.
pub(crate) fn projection_hash(rows: &[RunShardCaseDraft]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"vigilo/run-shard-cases/v1");
    for row in rows {
        update_projection_hash(&mut hasher, row);
    }
    hasher.finalize().to_hex().to_string()
}

fn update_projection_hash(hasher: &mut blake3::Hasher, row: &RunShardCaseDraft) {
    hasher.update(&row.run_shard.to_be_bytes());
    hasher.update(&row.case_ordinal.to_be_bytes());
    hasher.update(row.case_id.as_bytes());
    let hash_bytes = row.case_hash.as_bytes();
    hasher.update(&(hash_bytes.len() as u64).to_be_bytes());
    hasher.update(hash_bytes);
}

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

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed_projection_run(pool: &PgPool, run_id: Uuid, dataset_version_id: Uuid) {
        let dataset_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO dataset_versions (dataset_version_id, dataset_id, dataset_version) VALUES ($1, $2, 'test')",
        )
        .bind(dataset_version_id)
        .bind(dataset_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO runs (
                id, run_key, dataset_id, dataset_version_id, dataset_version,
                evaluation_profile_id, evaluation_profile_version,
                profile_version_id, profile_hash,
                aggregation_policy_id, aggregation_policy_version,
                aggregation_policy_hash, agent_provider, agent_name,
                prompt_config_id, prompt_config_version, expected_execution_count
            )
            VALUES (
                $1, $2, $3, $4, 'test', 'profile', '1.0.0',
                'profile-version', 'profile-hash', 'aggregate', '1.0.0',
                'aggregate-hash', 'test', 'agent', 'prompt', '1.0.0', 2
            )
            "#,
        )
        .bind(run_id)
        .bind(format!("run-{run_id}"))
        .bind(dataset_id)
        .bind(dataset_version_id)
        .execute(pool)
        .await
        .unwrap();
    }

    fn dataset_case(ordinal: i32) -> DatasetVersionCaseDraft {
        DatasetVersionCaseDraft {
            case_id: Uuid::from_u128(ordinal as u128 + 1),
            case_ordinal: ordinal,
            case_hash: format!("hash-{ordinal}"),
        }
    }

    fn chunk(run_shard: i16, start: i32, end: i32) -> RunChunkDraft {
        RunChunkDraft {
            chunk_id: Uuid::now_v7(),
            run_shard,
            profile_group_id: "default".to_string(),
            ordinal_start: start,
            ordinal_end: end,
        }
    }

    #[test]
    fn projection_contains_only_cases_owned_by_the_placement_chunks() {
        let run_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        let cases = (0..8).map(dataset_case).collect::<Vec<_>>();
        let chunks = vec![chunk(3, 0, 2), chunk(7, 5, 8)];

        let projected =
            project_cases_for_chunks(run_id, dataset_version_id, &cases, &chunks).unwrap();

        assert_eq!(
            projected
                .iter()
                .map(|row| (row.run_shard, row.case_ordinal))
                .collect::<Vec<_>>(),
            vec![(3, 0), (3, 1), (7, 5), (7, 6), (7, 7)]
        );
        assert!(
            projected
                .iter()
                .all(|row| row.run_id == run_id && row.dataset_version_id == dataset_version_id)
        );
    }

    #[test]
    fn projection_rejects_overlapping_chunk_ranges() {
        let error = project_cases_for_chunks(
            Uuid::now_v7(),
            Uuid::now_v7(),
            &(0..5).map(dataset_case).collect::<Vec<_>>(),
            &[chunk(1, 0, 3), chunk(2, 2, 5)],
        )
        .unwrap_err();

        assert!(error.to_string().contains("ordinal 2"));
    }

    #[test]
    fn projection_hash_is_stable_and_order_sensitive() {
        let run_id = Uuid::from_u128(10);
        let dataset_version_id = Uuid::from_u128(20);
        let cases = (0..3).map(dataset_case).collect::<Vec<_>>();
        let projection =
            project_cases_for_chunks(run_id, dataset_version_id, &cases, &[chunk(4, 0, 3)])
                .unwrap();
        let mut reversed = projection.clone();
        reversed.reverse();

        assert_eq!(projection_hash(&projection), projection_hash(&projection));
        assert_ne!(projection_hash(&projection), projection_hash(&reversed));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx projection tests"]
    async fn projection_fingerprint_is_scoped_to_requested_shards(pool: PgPool) {
        let run_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        seed_projection_run(&pool, run_id, dataset_version_id).await;
        let rows = [1_i16, 2_i16]
            .into_iter()
            .enumerate()
            .map(|(ordinal, run_shard)| RunShardCaseDraft {
                run_id,
                run_shard,
                dataset_version_id,
                case_id: Uuid::now_v7(),
                case_ordinal: ordinal as i32,
                case_hash: format!("hash-{run_shard}"),
            })
            .collect::<Vec<_>>();
        for row in &rows {
            sqlx::query(
                "INSERT INTO case_blobs (case_hash, task_type, input_payload, expected_output) VALUES ($1, 'test', '{}'::jsonb, 'null'::jsonb)",
            )
            .bind(&row.case_hash)
            .execute(&pool)
            .await
            .unwrap();
        }
        let mut tx = pool.begin().await.unwrap();
        insert_projection_page(&mut tx, &rows).await.unwrap();
        tx.commit().await.unwrap();

        let fingerprint = projection_fingerprint(&pool, run_id, &[1]).await.unwrap();

        assert_eq!(fingerprint, (1, projection_hash(&rows[..1])));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx projection tests"]
    async fn projection_page_replay_rejects_immutable_conflict(pool: PgPool) {
        let run_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        seed_projection_run(&pool, run_id, dataset_version_id).await;
        for case_hash in ["original-hash", "conflicting-hash"] {
            sqlx::query(
                "INSERT INTO case_blobs (case_hash, task_type, input_payload, expected_output) VALUES ($1, 'test', '{}'::jsonb, 'null'::jsonb)",
            )
            .bind(case_hash)
            .execute(&pool)
            .await
            .unwrap();
        }
        let original = RunShardCaseDraft {
            run_id,
            run_shard: 1,
            dataset_version_id,
            case_id: Uuid::now_v7(),
            case_ordinal: 0,
            case_hash: "original-hash".to_string(),
        };
        let mut tx = pool.begin().await.unwrap();
        insert_projection_page(&mut tx, std::slice::from_ref(&original))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut conflicting = original;
        conflicting.case_hash = "conflicting-hash".to_string();
        let mut tx = pool.begin().await.unwrap();

        let error = insert_projection_page(&mut tx, &[conflicting])
            .await
            .unwrap_err();

        assert!(error.to_string().contains("conflicts with persisted"));
    }
}
