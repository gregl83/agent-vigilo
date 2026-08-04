// PostgreSQL-backed workflow scenarios and fixtures.

use sqlx::PgPool;

use super::{
    tests::chunk,
    *,
};

fn case_blob(label: &str) -> CaseBlobDraft {
    CaseBlobDraft {
        case_hash: format!("case-{label}-{}", Uuid::now_v7()),
        task_type: "classification".to_string(),
        case_group: None,
        input_payload: serde_json::json!({"text": label}),
        expected_output: serde_json::Value::Null,
        context_payload: serde_json::Value::Null,
        tags: serde_json::json!([]),
        metadata: serde_json::json!({}),
    }
}

async fn insert_minimal_run(pool: &PgPool, run_id: Uuid) {
    let dataset_id = Uuid::now_v7();
    let dataset_version_id = Uuid::now_v7();

    sqlx::query(
        r#"
            INSERT INTO dataset_versions (dataset_version_id, dataset_id, dataset_version)
            VALUES ($1::uuid, $2::uuid, 'test')
            "#,
    )
    .bind(dataset_version_id)
    .bind(dataset_id)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        r#"
            INSERT INTO runs (
                id,
                run_key,
                dataset_id,
                dataset_version_id,
                dataset_version,
                evaluation_profile_id,
                evaluation_profile_version,
                profile_version_id,
                profile_hash,
                aggregation_policy_id,
                aggregation_policy_version,
                aggregation_policy_hash,
                agent_provider,
                agent_name,
                prompt_config_id,
                prompt_config_version,
                expected_execution_count
            )
            VALUES (
                $1::uuid,
                $2,
                $3::uuid,
                $4::uuid,
                'test',
                'profile',
                '1.0.0',
                'profile-version',
                'profile-hash',
                'aggregation',
                '1.0.0',
                'aggregation-hash',
                'example',
                'agent',
                'prompt',
                '1.0.0',
                3
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
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
async fn bulk_insert_run_shard_dispatch_cursors_inserts_distinct_open_shards(pool: PgPool) {
    let run_id = Uuid::now_v7();
    insert_minimal_run(&pool, run_id).await;

    let chunks = vec![chunk(0, 0), chunk(1, 1), chunk(1, 2)];
    let mut tx = pool.begin().await.unwrap();
    bulk_insert_run_shard_dispatch_cursors(&mut tx, run_id, &chunks)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let rows = sqlx::query_as::<_, (i16, String)>(
        r#"
            SELECT run_shard, status
            FROM run_shard_dispatch_cursors
            WHERE run_id = $1::uuid
            ORDER BY run_shard
            "#,
    )
    .bind(run_id)
    .fetch_all(&pool)
    .await
    .unwrap();

    assert_eq!(rows, vec![(0, "open".to_string()), (1, "open".to_string())]);
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
async fn bulk_insert_case_blobs_preserves_json_null_context(pool: PgPool) {
    let case_blob = CaseBlobDraft {
        case_hash: format!("case-{}", Uuid::now_v7()),
        task_type: "classification".to_string(),
        case_group: None,
        input_payload: serde_json::json!({"text": "hello"}),
        expected_output: serde_json::Value::Null,
        context_payload: serde_json::Value::Null,
        tags: serde_json::json!([]),
        metadata: serde_json::json!({}),
    };

    let mut tx = pool.begin().await.unwrap();
    bulk_insert_case_blobs(&mut tx, std::slice::from_ref(&case_blob))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let context_payload = sqlx::query_scalar::<_, serde_json::Value>(
        r#"
            SELECT context_payload
            FROM case_blobs
            WHERE case_hash = $1
            "#,
    )
    .bind(&case_blob.case_hash)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(context_payload, serde_json::Value::Null);
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
async fn bulk_insert_case_blobs_rejects_hash_content_mismatch(pool: PgPool) {
    let mut case_blob = CaseBlobDraft {
        case_hash: format!("case-{}", Uuid::now_v7()),
        task_type: "classification".to_string(),
        case_group: None,
        input_payload: serde_json::json!({"text": "original"}),
        expected_output: serde_json::Value::Null,
        context_payload: serde_json::Value::Null,
        tags: serde_json::json!([]),
        metadata: serde_json::json!({}),
    };
    let mut tx = pool.begin().await.unwrap();
    bulk_insert_case_blobs(&mut tx, std::slice::from_ref(&case_blob))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    case_blob.input_payload = serde_json::json!({"text": "different"});
    let mut tx = pool.begin().await.unwrap();
    let error = bulk_insert_case_blobs(&mut tx, &[case_blob])
        .await
        .unwrap_err();

    assert!(is_seed_invariant_error(&error));
    assert!(error.to_string().contains("different immutable content"));
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
async fn bulk_insert_dataset_membership_rejects_incomplete_seed(pool: PgPool) {
    let dataset_id = Uuid::now_v7();
    let dataset_version_id = Uuid::now_v7();
    let blobs = [case_blob("first"), case_blob("second")];
    let memberships = [
        DatasetVersionCaseDraft {
            case_id: Uuid::now_v7(),
            case_ordinal: 0,
            case_hash: blobs[0].case_hash.clone(),
        },
        DatasetVersionCaseDraft {
            case_id: Uuid::now_v7(),
            case_ordinal: 1,
            case_hash: blobs[1].case_hash.clone(),
        },
    ];
    let mut tx = pool.begin().await.unwrap();
    bulk_insert_case_blobs(&mut tx, &blobs).await.unwrap();
    upsert_dataset_version(&mut tx, dataset_version_id, dataset_id, "test")
        .await
        .unwrap();
    bulk_insert_dataset_membership(&mut tx, dataset_version_id, &memberships)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let error = bulk_insert_dataset_membership(
        &mut tx,
        dataset_version_id,
        std::slice::from_ref(&memberships[0]),
    )
    .await
    .unwrap_err();

    assert!(is_seed_invariant_error(&error));
    assert!(error.to_string().contains("persisted memberships"));
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
async fn bulk_insert_run_chunks_is_idempotent(pool: PgPool) {
    let run_id = Uuid::now_v7();
    insert_minimal_run(&pool, run_id).await;
    let dataset_version_id =
        sqlx::query_scalar::<_, Uuid>("SELECT dataset_version_id FROM runs WHERE id = $1::uuid")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let chunks = vec![chunk(0, 0)];

    for _ in 0..2 {
        let mut tx = pool.begin().await.unwrap();
        bulk_insert_run_chunks(&mut tx, run_id, dataset_version_id, &chunks)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::bigint FROM run_chunks WHERE run_id = $1::uuid",
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
async fn bulk_insert_shard_placements_inserts_distinct_active_shards(pool: PgPool) {
    let run_id = Uuid::now_v7();
    insert_minimal_run(&pool, run_id).await;

    let assignments = vec![
        RunShardPlacementAssignment {
            run_shard: 0,
            database_alias: "primary".to_string(),
        },
        RunShardPlacementAssignment {
            run_shard: 1,
            database_alias: "primary".to_string(),
        },
    ];
    let mut tx = pool.begin().await.unwrap();
    bulk_insert_shard_placements(&mut tx, run_id, &assignments)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let rows = sqlx::query_as::<_, (i16, String, String)>(
        r#"
            SELECT run_shard, database_alias, status
            FROM shard_placements
            WHERE run_id = $1::uuid
            ORDER BY run_shard
            "#,
    )
    .bind(run_id)
    .fetch_all(&pool)
    .await
    .unwrap();

    assert_eq!(
        rows,
        vec![
            (0, "primary".to_string(), "active".to_string()),
            (1, "primary".to_string(), "active".to_string())
        ]
    );
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx migration tests"]
async fn bulk_insert_shard_placements_rejects_control_only_alias(pool: PgPool) {
    let run_id = Uuid::now_v7();
    insert_minimal_run(&pool, run_id).await;

    sqlx::query(
        r#"
            UPDATE database_placements
            SET role = 'control'
            WHERE alias = 'primary'
            "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let assignments = vec![RunShardPlacementAssignment {
        run_shard: 0,
        database_alias: "primary".to_string(),
    }];
    let mut tx = pool.begin().await.unwrap();
    let error = bulk_insert_shard_placements(&mut tx, run_id, &assignments)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("not shard-capable"));
}
