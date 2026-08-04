use super::*;
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn add_shard_database_placement_requires_env_unless_deferred(pool: PgPool) {
    let error =
        add_shard_database_placement(&pool, "shard_001", "VIGILO_TEST_MISSING_SHARD_URL", false)
            .await
            .unwrap_err();
    assert!(error.to_string().contains("is not set"));
    let placement =
        add_shard_database_placement(&pool, "shard_001", "VIGILO_TEST_MISSING_SHARD_URL", true)
            .await
            .unwrap();
    assert_eq!(placement.alias, "shard_001");
    assert_eq!(placement.role, DATABASE_PLACEMENT_ROLE_SHARD);
    assert_eq!(placement.status, DATABASE_PLACEMENT_STATUS_ACTIVE);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn disable_database_placement_rejects_active_control(pool: PgPool) {
    let database_router = database_router_with_isolated_control_pool(pool).await;
    let error = disable_database_placement(&database_router, DEFAULT_DATABASE_ALIAS)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("control-capable and active"));
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn draining_database_placement_rejects_new_routes_but_keeps_existing_routes(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (run_id, run_shard, database_alias, status)             VALUES ($1::uuid, 3, 'shard_001', 'active')             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
    let placement = drain_database_placement(&database_router, "shard_001")
        .await
        .unwrap();
    assert_eq!(placement.status, DATABASE_PLACEMENT_STATUS_DRAINING);
    let repeated = drain_database_placement(&database_router, "shard_001")
        .await
        .unwrap();
    assert_eq!(repeated.updated_at, placement.updated_at);
    let persisted_route = shard_placements::select_shard_placement(&pool, run_id, 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted_route.database_alias, "shard_001");
    let error = set_shard_placement(&database_router, Uuid::now_v7(), 4, "shard_001")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot receive new shard ownership")
    );
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn projection_rows_prevent_direct_empty_shard_reassignment(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    let dataset_version_id = Uuid::now_v7();
    seed_run(&pool, run_id, Uuid::now_v7(), dataset_version_id).await;
    let case_id = Uuid::now_v7();
    sqlx::query(             "INSERT INTO case_blobs (case_hash, task_type, input_payload, expected_output) VALUES ('projection-only', 'test', '{}'::jsonb, 'null'::jsonb)",         )         .execute(&pool)         .await         .unwrap();
    sqlx::query(         r#"             INSERT INTO run_shard_cases (                 run_id, run_shard, dataset_version_id,                 case_id, case_ordinal, case_hash             )             VALUES ($1, 3, $2, $3, 0, 'projection-only')             "#,     )     .bind(run_id)     .bind(dataset_version_id)     .bind(case_id)     .execute(&pool)     .await     .unwrap();
    sqlx::query(             "INSERT INTO shard_placements (run_id, run_shard, database_alias, status) VALUES ($1, 3, 'primary', 'active')",         )         .bind(run_id)         .execute(&pool)         .await         .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool).await;
    let error = set_shard_placement(&database_router, run_id, 3, "shard_001")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already has 1 shard-owned row"));
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn disable_requires_draining_placement_without_owned_routes(pool: PgPool) {
    let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (run_id, run_shard, database_alias, status)             VALUES ($1::uuid, 3, 'shard_001', 'active')             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let active_error = disable_database_placement(&database_router, "shard_001")
        .await
        .unwrap_err();
    assert!(active_error.to_string().contains("must be draining"));
    drain_database_placement(&database_router, "shard_001")
        .await
        .unwrap();
    let owned_error = disable_database_placement(&database_router, "shard_001")
        .await
        .unwrap_err();
    assert!(owned_error.to_string().contains("still owns 1 shard route"));
    sqlx::query("DELETE FROM shard_placements WHERE database_alias = 'shard_001'")
        .execute(&pool)
        .await
        .unwrap();
    let disabled = disable_database_placement(&database_router, "shard_001")
        .await
        .unwrap();
    assert_eq!(disabled.status, DATABASE_PLACEMENT_STATUS_DISABLED);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn drain_rejects_database_referenced_by_inflight_move(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (                 run_id,                 run_shard,                 database_alias,                 status,                 move_target_database_alias,                 route_version             )             VALUES ($1::uuid, 3, 'primary', 'moving', 'shard_001', 3)             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
    let error = drain_database_placement(&database_router, "shard_001")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("target of 1 in-flight shard move"),
        "{error:#}"
    );
    let placement = database_placements::select_database_placement(&pool, "shard_001")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(placement.status, DATABASE_PLACEMENT_STATUS_ACTIVE);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn disable_rejects_database_referenced_by_inflight_move(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'draining')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (                 run_id,                 run_shard,                 database_alias,                 status,                 move_target_database_alias,                 route_version             )             VALUES ($1::uuid, 3, 'primary', 'moving', 'shard_001', 3)             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool).await;
    let error = disable_database_placement(&database_router, "shard_001")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("target of 1 in-flight shard move"),
        "{error:#}"
    );
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn concurrent_database_drain_wins_before_move_target_reservation(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (run_id, run_shard, database_alias, status)             VALUES ($1::uuid, 3, 'primary', 'active')             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let mut lifecycle_tx = pool.begin().await.unwrap();
    database_placements::select_database_placement_for_update(&mut lifecycle_tx, "shard_001")
        .await
        .unwrap()
        .unwrap();
    database_placements::update_database_placement_status(
        &mut lifecycle_tx,
        "shard_001",
        DATABASE_PLACEMENT_STATUS_DRAINING,
    )
    .await
    .unwrap()
    .unwrap();
    let reservation_pool = pool.clone();
    let mut reservation = tokio::spawn(async move {
        reserve_active_move_target_and_mark_copying(
            &reservation_pool,
            run_id,
            3,
            DEFAULT_DATABASE_ALIAS,
            1,
            "shard_001",
        )
        .await
    });
    tokio::select! {         result = &mut reservation => {             panic!("move target reservation bypassed the placement lifecycle lock: {result:?}");         }         () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}     }
    lifecycle_tx.commit().await.unwrap();
    let error = reservation.await.unwrap().unwrap_err();
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
    assert_eq!(route.status, SHARD_PLACEMENT_STATUS_ACTIVE);
    assert!(route.move_target_database_alias.is_none());
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn concurrent_move_target_reservation_wins_before_database_drain(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (run_id, run_shard, database_alias, status)             VALUES ($1::uuid, 3, 'primary', 'active')             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let mut reservation_tx = pool.begin().await.unwrap();
    validate_new_ownership_target(&mut reservation_tx, "shard_001")
        .await
        .unwrap();
    shard_placements::mark_shard_placement_copying(
        &mut *reservation_tx,
        run_id,
        3,
        DEFAULT_DATABASE_ALIAS,
        1,
        "shard_001",
    )
    .await
    .unwrap()
    .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
    let mut drain =
        tokio::spawn(async move { drain_database_placement(&database_router, "shard_001").await });
    tokio::select! {         result = &mut drain => {             panic!("database drain bypassed the move target reservation: {result:?}");         }         () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}     }
    reservation_tx.commit().await.unwrap();
    let error = drain.await.unwrap().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("target of 1 in-flight shard move"),
        "{error:#}"
    );
    let placement = database_placements::select_database_placement(&pool, "shard_001")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(placement.status, DATABASE_PLACEMENT_STATUS_ACTIVE);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn concurrent_drain_serializes_before_move_activation(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (                 run_id,                 run_shard,                 database_alias,                 status,                 move_target_database_alias,                 route_version             )             VALUES ($1::uuid, 3, 'primary', 'moving', 'shard_001', 3)             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let mut lifecycle_tx = pool.begin().await.unwrap();
    database_placements::select_database_placement_for_update(&mut lifecycle_tx, "shard_001")
        .await
        .unwrap()
        .unwrap();
    let activation_pool = pool.clone();
    let mut activation = tokio::spawn(async move {
        activate_moved_shard_placement_on_target(
            &activation_pool,
            run_id,
            3,
            "primary",
            3,
            "shard_001",
        )
        .await
    });
    tokio::select! {         result = &mut activation => {             panic!("move activation bypassed the placement lifecycle lock: {result:?}");         }         () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}     }
    database_placements::update_database_placement_status(
        &mut lifecycle_tx,
        "shard_001",
        DATABASE_PLACEMENT_STATUS_DRAINING,
    )
    .await
    .unwrap()
    .unwrap();
    lifecycle_tx.commit().await.unwrap();
    let error = activation.await.unwrap().unwrap_err();
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
async fn disable_rejects_pending_outbox_delivery(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'draining')             "#,     )     .execute(&pool)     .await     .unwrap();
    sqlx::query(         r#"             INSERT INTO outbox_events (                 event_type,                 aggregate_type,                 aggregate_id,                 dedupe_key             )             VALUES ('test.event', 'test', $1::uuid, $2)             "#,     )     .bind(Uuid::now_v7())     .bind(format!("drain-test-{}", Uuid::now_v7()))     .execute(&pool)     .await     .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool).await;
    let error = disable_database_placement(&database_router, "shard_001")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("pending outbox delivery"));
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn inspect_shard_route_keeps_draining_owner_dispatchable(pool: PgPool) {
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'draining')             "#,     )     .execute(&pool)     .await     .unwrap();
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (run_id, run_shard, database_alias, status)             VALUES ($1::uuid, 4, 'shard_001', 'active')             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let database_router = database_router_with_isolated_control_pool(pool).await;
    let route = inspect_shard_route(&database_router, run_id, 4)
        .await
        .unwrap();
    assert!(route.dispatchable);
    assert!(route.readable);
    assert_eq!(route.routing_decision, "dispatchable");
    assert_eq!(route.database_status, DATABASE_PLACEMENT_STATUS_DRAINING);
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn inspect_shard_route_reports_dispatchable_primary_route(pool: PgPool) {
    let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO shard_placements (run_id, run_shard, database_alias, status)             VALUES ($1::uuid, 4, 'primary', 'active')             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let route = inspect_shard_route(&database_router, run_id, 4)
        .await
        .unwrap();
    assert_eq!(route.database_alias, "primary");
    assert_eq!(route.database_role, "control_and_shard");
    assert_eq!(route.database_url_env, DEFAULT_DATABASE_URL_ENV);
    assert!(route.database_url_env_resolved);
    assert!(route.dispatchable);
    assert!(route.readable);
    assert_eq!(route.routing_decision, "dispatchable");
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn inspect_shard_route_reports_moving_route_as_read_only(pool: PgPool) {
    let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    sqlx::query(         r#"             INSERT INTO shard_placements (                 run_id,                 run_shard,                 database_alias,                 status,                 move_target_database_alias             )             VALUES ($1::uuid, 4, 'primary', 'moving', 'shard_001')             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let move_id = sqlx::query_scalar::<_, Uuid>(         r#"             INSERT INTO shard_move_operations (                 run_id, run_shard, source_database_alias,                 target_database_alias, starting_route_version,                 phase, copied_row_count, copied_byte_count             )             VALUES ($1, 4, 'primary', 'shard_001', 1, 'cutover', 3, 512)             RETURNING id             "#,     )     .bind(run_id)     .fetch_one(&pool)     .await     .unwrap();
    sqlx::query(         r#"             INSERT INTO shard_move_table_progress (                 move_id, table_name, completed_page_count,                 last_end_key, copied_row_count, copied_byte_count,                 last_page_checksum             )             VALUES ($1, 'run_chunks', 1, 'end', 3, 512, 'checksum')             "#,     )     .bind(move_id)     .execute(&pool)     .await     .unwrap();
    let route = inspect_shard_route(&database_router, run_id, 4)
        .await
        .unwrap();
    assert!(!route.dispatchable);
    assert!(route.readable);
    assert_eq!(route.routing_decision, "read_only");
    assert_eq!(route.move_operation_id, Some(move_id));
    assert_eq!(route.move_phase.as_deref(), Some("cutover"));
    assert_eq!(route.move_completed_page_count, Some(1));
    assert_eq!(route.move_copied_row_count, Some(3));
    assert_eq!(route.move_copied_byte_count, Some(512));
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn inspect_shard_route_reports_disabled_placement_as_blocked(pool: PgPool) {
    let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_disabled', 'VIGILO_TEST_MISSING_SHARD_URL', 'shard', 'disabled')             "#,     )     .execute(&pool)     .await     .unwrap();
    sqlx::query(         r#"             INSERT INTO shard_placements (run_id, run_shard, database_alias, status)             VALUES ($1::uuid, 4, 'shard_disabled', 'active')             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let route = inspect_shard_route(&database_router, run_id, 4)
        .await
        .unwrap();
    assert_eq!(route.database_alias, "shard_disabled");
    assert_eq!(route.database_status, "disabled");
    assert!(!route.database_url_env_resolved);
    assert!(!route.dispatchable);
    assert!(!route.readable);
    assert_eq!(route.routing_decision, "blocked");
}
#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx shard admin tests"]
async fn creating_run_rejects_route_changes(pool: PgPool) {
    let database_router = database_router_with_isolated_control_pool(pool.clone()).await;
    let run_id = Uuid::now_v7();
    sqlx::query(         r#"             INSERT INTO database_placements (alias, database_url_env, role, status)             VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')             "#,     )     .execute(&pool)     .await     .unwrap();
    seed_run(&pool, run_id, Uuid::now_v7(), Uuid::now_v7()).await;
    sqlx::query("UPDATE runs SET status = 'creating'::run_status WHERE id = $1::uuid")
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(         r#"             INSERT INTO shard_placements (run_id, run_shard, database_alias, status)             VALUES ($1::uuid, 3, 'primary', 'active')             "#,     )     .bind(run_id)     .execute(&pool)     .await     .unwrap();
    let set_error = set_shard_placement(&database_router, run_id, 3, "shard_001")
        .await
        .unwrap_err();
    let move_error = move_shard_placement(
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
    .unwrap_err();
    assert!(set_error.to_string().contains("still creating"));
    assert!(move_error.to_string().contains("still creating"));
}
