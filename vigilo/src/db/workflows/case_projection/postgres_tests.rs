// PostgreSQL-backed workflow scenarios and fixtures.

use sqlx::PgPool;

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
