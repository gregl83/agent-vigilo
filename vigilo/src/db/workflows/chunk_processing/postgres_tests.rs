// PostgreSQL-backed workflow scenarios and fixtures.

use sqlx::PgPool;

use super::*;

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx workflow tests"]
async fn case_batch_rejects_an_incomplete_run_shard_projection(pool: PgPool) {
    let run_id = Uuid::now_v7();
    let dataset_id = Uuid::now_v7();
    let dataset_version_id = Uuid::now_v7();
    let first_case_id = Uuid::now_v7();
    let second_case_id = Uuid::now_v7();
    sqlx::query(
            "INSERT INTO dataset_versions (dataset_version_id, dataset_id, dataset_version) VALUES ($1, $2, 'test')",
        )
        .bind(dataset_version_id)
        .bind(dataset_id)
        .execute(&pool)
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
    .execute(&pool)
    .await
    .unwrap();
    for (case_id, ordinal) in [(first_case_id, 0), (second_case_id, 1)] {
        let case_hash = format!("hash-{ordinal}");
        sqlx::query(
            r#"
                INSERT INTO case_blobs (
                    case_hash, task_type, input_payload, expected_output
                )
                VALUES ($1, 'classification', '{}'::jsonb, 'null'::jsonb)
                "#,
        )
        .bind(&case_hash)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
                INSERT INTO dataset_version_cases (
                    dataset_version_id, case_id, case_ordinal, case_hash
                )
                VALUES ($1, $2, $3, $4)
                "#,
        )
        .bind(dataset_version_id)
        .bind(case_id)
        .bind(ordinal)
        .bind(&case_hash)
        .execute(&pool)
        .await
        .unwrap();
        if ordinal == 1 {
            sqlx::query(
                r#"
                    INSERT INTO run_shard_cases (
                        run_id, run_shard, dataset_version_id,
                        case_id, case_ordinal, case_hash
                    )
                    VALUES ($1, 7, $2, $3, $4, $5)
                    "#,
            )
            .bind(run_id)
            .bind(dataset_version_id)
            .bind(case_id)
            .bind(ordinal)
            .bind(&case_hash)
            .execute(&pool)
            .await
            .unwrap();
        }
    }
    let chunk = RunChunk {
        write_epoch: 1,
        id: Uuid::now_v7(),
        run_id,
        run_shard: 7,
        dataset_version_id,
        profile_group_id: "default".to_string(),
        ordinal_start: 0,
        ordinal_end: 2,
        status: "leased".to_string(),
        lease_token: Some(Uuid::now_v7()),
        leased_until: Some(chrono::Utc::now() + chrono::Duration::minutes(1)),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    let error = load_chunk_case_batch(&pool, &chunk).await.unwrap_err();

    assert!(error.to_string().contains("expected case ordinal 0"));
}
