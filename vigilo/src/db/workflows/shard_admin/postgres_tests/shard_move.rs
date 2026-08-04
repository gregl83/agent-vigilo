use super::*;
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn move_table_progress_is_compact_and_idempotent(pool: PgPool) {
    sqlx::query(             "INSERT INTO database_placements (alias, database_url_env, role, status) VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')",         )         .execute(&pool)         .await         .unwrap();
    let run_id = Uuid::now_v7();
    let claim_token = Uuid::now_v7();
    let move_id = sqlx::query_scalar::<_, Uuid>(         r#"             INSERT INTO shard_move_operations (                 run_id, run_shard, source_database_alias,                 target_database_alias, starting_route_version,                 claim_token, claimed_until             )             VALUES (                 $1, 3, 'primary', 'shard_001', 1,                 $2, now() + interval '5 minutes'             )             RETURNING id             "#,     )     .bind(run_id)     .bind(claim_token)     .fetch_one(&pool)     .await     .unwrap();
    let first = MoveSourceRow {
        row: serde_json::json!({"id": 1}),
        row_key: "key-1".to_string(),
        row_bytes: 16,
    };
    let second = MoveSourceRow {
        row: serde_json::json!({"id": 2}),
        row_key: "key-2".to_string(),
        row_bytes: 16,
    };
    record_completed_move_page(
        &pool,
        move_id,
        claim_token,
        "run_chunks",
        0,
        None,
        std::slice::from_ref(&first),
    )
    .await
    .unwrap();
    record_completed_move_page(
        &pool,
        move_id,
        claim_token,
        "run_chunks",
        0,
        None,
        std::slice::from_ref(&first),
    )
    .await
    .unwrap();
    record_completed_move_page(
        &pool,
        move_id,
        claim_token,
        "run_chunks",
        1,
        Some(&first.row_key),
        &[second],
    )
    .await
    .unwrap();
    let (progress_rows, completed_pages, copied_rows, operation_rows) =         sqlx::query_as::<_, (i64, i64, i64, i64)>(             r#"                 SELECT                     COUNT(*)::bigint,                     MAX(completed_page_count),                     MAX(copied_row_count),                     (                         SELECT copied_row_count                         FROM shard_move_operations                         WHERE id = $1                     )                 FROM shard_move_table_progress                 WHERE move_id = $1                   AND table_name = 'run_chunks'                 "#,         )         .bind(move_id)         .fetch_one(&pool)         .await         .unwrap();
    assert_eq!(progress_rows, 1);
    assert_eq!(completed_pages, 2);
    assert_eq!(copied_rows, 2);
    assert_eq!(operation_rows, 2);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn move_capture_coalesces_repeated_shard_mutations(pool: PgPool) {
    let run_id = Uuid::now_v7();
    let dataset_id = Uuid::now_v7();
    let dataset_version_id = Uuid::now_v7();
    let chunk_id = Uuid::now_v7();
    let move_id = Uuid::now_v7();
    seed_run(&pool, run_id, dataset_id, dataset_version_id).await;
    sqlx::query("INSERT INTO shard_move_captures (move_id, run_id, run_shard) VALUES ($1, $2, 3)")
        .bind(move_id)
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(         r#"             INSERT INTO run_chunks (                 id, run_id, run_shard, dataset_version_id,                 profile_group_id, ordinal_start, ordinal_end             )             VALUES ($1, $2, 3, $3, 'default', 0, 1)             "#,     )     .bind(chunk_id)     .bind(run_id)     .bind(dataset_version_id)     .execute(&pool)     .await     .unwrap();
    sqlx::query(         r#"             UPDATE run_chunks             SET recovery_count = recovery_count + 1             WHERE run_id = $1 AND run_shard = 3 AND id = $2             "#,     )     .bind(run_id)     .bind(chunk_id)     .execute(&pool)     .await     .unwrap();
    let (table_name, row_key, version) = sqlx::query_as::<_, (String, Value, i64)>(         r#"                 SELECT table_name, row_key, change_version                 FROM shard_move_dirty_keys                 WHERE move_id = $1                 "#,     )     .bind(move_id)     .fetch_one(&pool)     .await     .unwrap();
    assert_eq!(table_name, "run_chunks");
    assert_eq!(row_key["run_id"], run_id.to_string());
    assert_eq!(row_key["run_shard"], 3);
    assert_eq!(row_key["id"], chunk_id.to_string());
    assert_eq!(version, 2);
    let stale_key = DirtyShardKey {
        table_name: table_name.clone(),
        row_key: row_key.clone(),
        change_version: version - 1,
    };
    settle_replayed_dirty_keys(&pool, move_id, &[stale_key])
        .await
        .unwrap();
    let retained_version = sqlx::query_scalar::<_, i64>(
        "SELECT change_version FROM shard_move_dirty_keys WHERE move_id = $1",
    )
    .bind(move_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retained_version, version);
    let current_key = DirtyShardKey {
        table_name,
        row_key,
        change_version: version,
    };
    settle_replayed_dirty_keys(&pool, move_id, &[current_key])
        .await
        .unwrap();
    let settled_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::bigint FROM shard_move_dirty_keys WHERE move_id = $1",
    )
    .bind(move_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(settled_count, 0);
    sqlx::query(             "UPDATE run_chunks SET recovery_count = recovery_count + 1 WHERE run_id = $1 AND run_shard = 3 AND id = $2",         )         .bind(run_id)         .bind(chunk_id)         .execute(&pool)         .await         .unwrap();
    sqlx::query("DELETE FROM run_chunks WHERE run_id = $1 AND run_shard = 3 AND id = $2")
        .bind(run_id)
        .bind(chunk_id)
        .execute(&pool)
        .await
        .unwrap();
    let existing = select_dirty_shard_keys_for_table(
        &pool,
        move_id,
        "run_chunks",
        &["run_id", "run_shard", "id"],
        true,
        10,
    )
    .await
    .unwrap();
    let deleted = select_dirty_shard_keys_for_table(
        &pool,
        move_id,
        "run_chunks",
        &["run_id", "run_shard", "id"],
        false,
        10,
    )
    .await
    .unwrap();
    assert!(existing.is_empty());
    assert_eq!(deleted.len(), 1);
    sqlx::query("DELETE FROM shard_move_captures WHERE move_id = $1")
        .bind(move_id)
        .execute(&pool)
        .await
        .unwrap();
    let dirty_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::bigint FROM shard_move_dirty_keys WHERE move_id = $1",
    )
    .bind(move_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(dirty_count, 0);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn abort_fences_a_prepared_move_before_copying_starts(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(             "INSERT INTO shard_placements (run_id, run_shard, database_alias, status) VALUES ($1, 3, 'primary', 'active')",         )         .bind(run_id)         .execute(&pool)         .await         .unwrap();
    let move_id = sqlx::query_scalar::<_, Uuid>(         r#"             INSERT INTO shard_move_operations (                 run_id, run_shard, source_database_alias,                 target_database_alias, starting_route_version             )             VALUES ($1, 3, 'primary', 'shard_001', 1)             RETURNING id             "#,     )     .bind(run_id)     .fetch_one(&pool)     .await     .unwrap();
    sqlx::query("INSERT INTO shard_move_captures (move_id, run_id, run_shard) VALUES ($1, $2, 3)")
        .bind(move_id)
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
    let outcome = abort_shard_move(&database_router, run_id, 3, "primary", "shard_001")
        .await
        .unwrap();
    assert!(outcome.aborted);
    assert_eq!(outcome.placement.status, SHARD_PLACEMENT_STATUS_ACTIVE);
    assert_eq!(outcome.placement.route_version, 2);
    let operation_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM shard_move_operations WHERE id = $1")
            .bind(move_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(operation_status, "aborted");
    assert!(             shard_placements::mark_shard_placement_copying(                 &pool,                 run_id,                 3,                 "primary",                 1,                 "shard_001",             )             .await             .unwrap()             .is_none()         );
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn completed_route_retry_settles_ambiguous_cutover_state(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(             "INSERT INTO shard_placements (run_id, run_shard, database_alias, status, route_version) VALUES ($1, 3, 'shard_001', 'active', 5)",         )         .bind(run_id)         .execute(&pool)         .await         .unwrap();
    let move_id = sqlx::query_scalar::<_, Uuid>(         r#"             INSERT INTO shard_move_operations (                 run_id, run_shard, source_database_alias,                 target_database_alias, starting_route_version, phase             )             VALUES ($1, 3, 'primary', 'shard_001', 1, 'cutover')             RETURNING id             "#,     )     .bind(run_id)     .fetch_one(&pool)     .await     .unwrap();
    sqlx::query("INSERT INTO shard_move_captures (move_id, run_id, run_shard) VALUES ($1, $2, 3)")
        .bind(move_id)
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
    let outcome = move_shard_placement(
        &database_router,
        run_id,
        3,
        "shard_001",
        ShardMoveOptions {
            dry_run: false,
            verify_only: false,
            force: false,
        },
    )
    .await
    .unwrap();
    assert!(!outcome.moved);
    let (operation_status, capture_count) = sqlx::query_as::<_, (String, i64)>(         r#"                 SELECT                     status,                     (SELECT COUNT(*)::bigint FROM shard_move_captures WHERE move_id = $1)                 FROM shard_move_operations                 WHERE id = $1                 "#,     )     .bind(move_id)     .fetch_one(&pool)     .await     .unwrap();
    assert_eq!(operation_status, "completed");
    assert_eq!(capture_count, 0);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn move_to_current_database_is_not_treated_as_a_completed_retry(pool: PgPool) {
    let run_id = Uuid::now_v7();
    sqlx::query(             "INSERT INTO shard_placements (run_id, run_shard, database_alias, status) VALUES ($1, 3, 'primary', 'active')",         )         .bind(run_id)         .execute(&pool)         .await         .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool).await;
    let error = move_shard_placement(
        &database_router,
        run_id,
        3,
        "primary",
        ShardMoveOptions {
            dry_run: false,
            verify_only: false,
            force: false,
        },
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("already routes to database placement primary")
    );
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn move_activation_rejects_target_changed_outside_lifecycle_workflow(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (                 run_id,                 run_shard,                 database_alias,                 status,                 move_target_database_alias,                 route_version             )             VALUES ($1::uuid, 3, 'primary', 'moving', 'shard_001', 3)             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    sqlx::query(         r#"             UPDATE database_placements             SET status = 'draining',                 updated_at = now()             WHERE alias = 'shard_001'             "#,     )     .execute(&pool)     .await     .unwrap();
    let error =
        activate_moved_shard_placement_on_target(&pool, run_id, 3, "primary", 3, "shard_001")
            .await
            .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot receive new shard ownership"),
        "{error:#}"
    );
    let route = shard_placements::select_shard_placement(&pool, run_id, 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(route.database_alias, "primary");
    assert_eq!(route.status, SHARD_PLACEMENT_STATUS_MOVING);
    assert_eq!(route.route_version, 3);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn abort_draining_shard_move_restores_source_route_idempotently(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (                 run_id,                 run_shard,                 database_alias,                 status,                 move_target_database_alias,                 route_version             )             VALUES ($1::uuid, 3, 'primary', 'draining', 'shard_001', 2)             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool).await;
    let first = abort_shard_move(
        &database_router,
        run_id,
        3,
        DEFAULT_DATABASE_ALIAS,
        "shard_001",
    )
    .await
    .unwrap();
    assert!(first.aborted);
    assert_eq!(first.placement.database_alias, DEFAULT_DATABASE_ALIAS);
    assert_eq!(first.placement.status, SHARD_PLACEMENT_STATUS_ACTIVE);
    assert!(first.placement.move_target_database_alias.is_none());
    assert_eq!(first.placement.route_version, 3);
    let repeated = abort_shard_move(
        &database_router,
        run_id,
        3,
        DEFAULT_DATABASE_ALIAS,
        "shard_001",
    )
    .await
    .unwrap();
    assert!(!repeated.aborted);
    assert_eq!(repeated.placement.route_version, 3);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn abort_moving_shard_move_restores_source_route(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (                 run_id,                 run_shard,                 database_alias,                 status,                 move_target_database_alias,                 route_version             )             VALUES ($1::uuid, 3, 'primary', 'moving', 'shard_001', 3)             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool).await;
    let outcome = abort_shard_move(
        &database_router,
        run_id,
        3,
        DEFAULT_DATABASE_ALIAS,
        "shard_001",
    )
    .await
    .unwrap();
    assert!(outcome.aborted);
    assert_eq!(outcome.placement.database_alias, DEFAULT_DATABASE_ALIAS);
    assert_eq!(outcome.placement.status, SHARD_PLACEMENT_STATUS_ACTIVE);
    assert!(outcome.placement.move_target_database_alias.is_none());
    assert_eq!(outcome.placement.route_version, 4);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn abort_rejects_route_that_completed_on_target(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (                 run_id,                 run_shard,                 database_alias,                 status,                 route_version             )             VALUES ($1::uuid, 3, 'shard_001', 'active', 4)             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool).await;
    let error = abort_shard_move(
        &database_router,
        run_id,
        3,
        DEFAULT_DATABASE_ALIAS,
        "shard_001",
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("already completed on database placement shard_001"),
        "{error:#}"
    );
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn stale_mover_cannot_restart_after_abort_advances_route_fence(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (                 run_id,                 run_shard,                 database_alias,                 status,                 move_target_database_alias,                 route_version             )             VALUES ($1::uuid, 3, 'primary', 'draining', 'shard_001', 2)             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let mut abort_tx = pool.begin().await.unwrap();
    crate::db::shard_write_fence::lock_exclusive(&mut abort_tx, run_id, 3)
        .await
        .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
    let mover = tokio::spawn(async move {
        move_shard_placement(
            &database_router,
            run_id,
            3,
            "shard_001",
            ShardMoveOptions {
                dry_run: false,
                verify_only: false,
                force: false,
            },
        )
        .await
    });
    wait_for_waiting_advisory_lock(&pool, "mover").await;
    shard_placements::abort_shard_placement_move(
        &mut *abort_tx,
        run_id,
        3,
        DEFAULT_DATABASE_ALIAS,
        2,
        "shard_001",
    )
    .await
    .unwrap()
    .unwrap();
    abort_tx.commit().await.unwrap();
    let error = mover.await.unwrap().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("route changed while waiting for move admission"),
        "{error:#}"
    );
    let route = shard_placements::select_shard_placement(&pool, run_id, 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(route.database_alias, DEFAULT_DATABASE_ALIAS);
    assert_eq!(route.status, SHARD_PLACEMENT_STATUS_ACTIVE);
    assert!(route.move_target_database_alias.is_none());
    assert_eq!(route.route_version, 3);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn stale_abort_cannot_cancel_newer_move_route_version(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (                 run_id,                 run_shard,                 database_alias,                 status,                 move_target_database_alias,                 route_version             )             VALUES ($1::uuid, 3, 'primary', 'draining', 'shard_001', 2)             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let mut move_tx = pool.begin().await.unwrap();
    crate::db::shard_write_fence::lock_exclusive(&mut move_tx, run_id, 3)
        .await
        .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
    let abort = tokio::spawn(async move {
        abort_shard_move(
            &database_router,
            run_id,
            3,
            DEFAULT_DATABASE_ALIAS,
            "shard_001",
        )
        .await
    });
    wait_for_waiting_advisory_lock(&pool, "abort").await;
    shard_placements::mark_shard_placement_moving(
        &mut *move_tx,
        run_id,
        3,
        DEFAULT_DATABASE_ALIAS,
        2,
        "shard_001",
    )
    .await
    .unwrap()
    .unwrap();
    move_tx.commit().await.unwrap();
    let error = abort.await.unwrap().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("route changed while waiting for move-abort admission"),
        "{error:#}"
    );
    let route = shard_placements::select_shard_placement(&pool, run_id, 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(route.status, SHARD_PLACEMENT_STATUS_MOVING);
    assert_eq!(route.route_version, 3);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn move_shard_placement_switches_alias_after_verification(pool: PgPool) {
    let database_url = isolated_database_url(&pool).await;
    let database_router = database_router_with_control_pool(pool.clone(), database_url);
    let run_id = Uuid::now_v7();
    let dataset_id = Uuid::now_v7();
    let dataset_version_id = Uuid::now_v7();
    let chunk_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    seed_run(&pool, run_id, dataset_id, dataset_version_id).await;
    sqlx::query(         r#"             INSERT INTO shard_placements (run_id, run_shard, database_alias, status)             VALUES ($1::uuid, 3, 'primary', 'active')             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    seed_run_snapshot(&pool, run_id, 3, dataset_id, dataset_version_id).await;
    sqlx::query(         r#"             INSERT INTO run_chunks (                 id,                 run_id,                 run_shard,                 dataset_version_id,                 profile_group_id,                 ordinal_start,                 ordinal_end,                 status             )             VALUES ($1::uuid, $2::uuid, 3, $3::uuid, 'default', 0, 1, 'completed')             "#,     )     .bind(chunk_id)     .bind(run_id)     .bind(dataset_version_id)     .execute(&pool)     .await     .unwrap();
    let outcome = move_shard_placement(
        &database_router,
        run_id,
        3,
        "shard_001",
        ShardMoveOptions {
            dry_run: false,
            verify_only: false,
            force: false,
        },
    )
    .await
    .unwrap();
    assert!(outcome.moved);
    assert!(outcome.tables.iter().all(|table| table.verified));
    assert_eq!(outcome.placement.database_alias, "shard_001");
    assert_eq!(outcome.placement.status, SHARD_PLACEMENT_STATUS_ACTIVE);
    assert!(outcome.placement.move_target_database_alias.is_none());
    assert_eq!(outcome.placement.route_version, 5);
    assert_eq!(outcome.placement.write_epoch, 2);
    let admission = sqlx::query_as::<_, (String, i64, String)>(             "SELECT database_alias, write_epoch, state FROM local_shard_admissions WHERE run_id = $1 AND run_shard = 3",         )         .bind(run_id)         .fetch_one(&pool)         .await         .unwrap();
    assert_eq!(admission, ("shard_001".to_string(), 2, "open".to_string()));
    let retried = move_shard_placement(
        &database_router,
        run_id,
        3,
        "shard_001",
        ShardMoveOptions {
            dry_run: false,
            verify_only: false,
            force: false,
        },
    )
    .await
    .unwrap();
    assert!(!retried.moved);
    assert!(retried.tables.iter().all(|table| table.verified));
    assert_eq!(retried.placement.database_alias, "shard_001");
    assert_eq!(retried.placement.route_version, 5);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn force_cannot_move_a_shard_with_active_work(pool: PgPool) {
    let database_url = isolated_database_url(&pool).await;
    let database_router = database_router_with_control_pool(pool.clone(), database_url);
    let run_id = Uuid::now_v7();
    let dataset_id = Uuid::now_v7();
    let dataset_version_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES                 ('shard_001', 'DATABASE_URL', 'shard', 'active'),                 ('shard_002', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    seed_run(&pool, run_id, dataset_id, dataset_version_id).await;
    seed_run_snapshot(&pool, run_id, 3, dataset_id, dataset_version_id).await;
    sqlx::query(         r#"             INSERT INTO shard_placements (run_id, run_shard, database_alias, status)             VALUES ($1::uuid, 3, 'primary', 'active')             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    sqlx::query(         r#"             INSERT INTO run_chunks (                 id, run_id, run_shard, dataset_version_id, profile_group_id,                 ordinal_start, ordinal_end, status, lease_token, leased_until             )             VALUES (                 $1::uuid, $2::uuid, 3, $3::uuid, 'default',                 0, 1, 'leased', gen_random_uuid(), now() + interval '1 minute'             )             "#,     )     .bind(Uuid::now_v7())     .bind(run_id)     .bind(dataset_version_id)     .execute(&pool)     .await     .unwrap();
    let error = move_shard_placement(
        &database_router,
        run_id,
        3,
        "shard_001",
        ShardMoveOptions {
            dry_run: false,
            verify_only: false,
            force: true,
        },
    )
    .await
    .unwrap_err();
    let placement = shard_placements::select_shard_placement(&pool, run_id, 3)
        .await
        .unwrap()
        .unwrap();
    assert!(error.to_string().contains("--force cannot bypass"));
    assert_eq!(placement.database_alias, "primary");
    assert_eq!(placement.status, SHARD_PLACEMENT_STATUS_DRAINING);
    assert_eq!(
        placement.move_target_database_alias.as_deref(),
        Some("shard_001")
    );
    assert_eq!(placement.route_version, 3);
    let redirect_error = move_shard_placement(
        &database_router,
        run_id,
        3,
        "shard_002",
        ShardMoveOptions {
            dry_run: false,
            verify_only: false,
            force: false,
        },
    )
    .await
    .unwrap_err();
    assert!(
        redirect_error
            .to_string()
            .contains("retry with the persisted target")
    );
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn copy_case_blob_rows_preserves_json_null_context(pool: PgPool) {
    let case_hash = format!("case-{}", Uuid::now_v7());
    let source_rows = vec![
        serde_json::json!({         "case_hash": case_hash,         "task_type": "classification",         "case_group": null,         "input_payload": {"text": "hello"},         "expected_output": null,         "context_payload": null,         "tags": [],         "metadata": {},         "created_at": Utc::now(),     }),
    ];
    copy_json_rows(&pool, "case_blobs", source_rows)
        .await
        .unwrap();
    let context_payload = sqlx::query_scalar::<_, Value>(         r#"             SELECT context_payload             FROM case_blobs             WHERE case_hash = $1             "#,     )     .bind(case_hash)     .fetch_one(&pool)     .await     .unwrap();
    assert_eq!(context_payload, Value::Null);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn prerequisite_fingerprints_ignore_local_timestamps_and_run_lifecycle(pool: PgPool) {
    let run_id = Uuid::now_v7();
    let dataset_id = Uuid::now_v7();
    let dataset_version_id = Uuid::now_v7();
    let case_id = Uuid::now_v7();
    let case_hash = format!("case-{}", Uuid::now_v7());
    seed_run(&pool, run_id, dataset_id, dataset_version_id).await;
    seed_case(&pool, dataset_version_id, case_id, &case_hash).await;
    let before = prerequisite_fingerprints(&pool, run_id).await;
    sqlx::query(         r#"             UPDATE case_blobs             SET created_at = created_at + interval '1 second'             WHERE case_hash = $1             "#,     )     .bind(&case_hash)     .execute(&pool)     .await     .unwrap();
    sqlx::query(         r#"             UPDATE dataset_versions             SET created_at = created_at + interval '1 second',                 updated_at = updated_at + interval '1 second'             WHERE dataset_version_id = $1::uuid             "#,     )     .bind(dataset_version_id)     .execute(&pool)     .await     .unwrap();
    sqlx::query(         r#"             UPDATE dataset_version_cases             SET created_at = created_at + interval '1 second',                 updated_at = updated_at + interval '1 second'             WHERE dataset_version_id = $1::uuid               AND case_id = $2::uuid             "#,     )     .bind(dataset_version_id)     .bind(case_id)     .execute(&pool)     .await     .unwrap();
    sqlx::query(         r#"             UPDATE runs             SET status = 'completed'::run_status,                 gate_status = 'pass'::gate_status,                 terminal_execution_count = 1,                 passed_execution_count = 1,                 summary = '{"local":"changed"}'::jsonb,                 completed_at = now(),                 updated_at = now()             WHERE id = $1::uuid             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let after = prerequisite_fingerprints(&pool, run_id).await;
    assert_eq!(before, after);
}
