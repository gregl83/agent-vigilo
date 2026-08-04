// PostgreSQL-backed workflow scenarios and fixtures.

use tokio::sync::OnceCell;

use super::*;
use crate::context::database::{
    CircuitBreakerConfig,
    DatabaseCircuitBreakers,
    DatabaseRouter,
    PlacementConfig,
    new_shard_placement_cache,
};

fn database_router_with_control_pool(pool: sqlx::PgPool) -> DatabaseRouter {
    let database_router = DatabaseRouter {
        uri: "postgres://injected-control-pool".to_string(),
        max_connections_per_pool: 5,
        acquire_timeout: std::time::Duration::from_secs(10),
        placement_config: PlacementConfig::default_single_database(),
        circuit_breakers: DatabaseCircuitBreakers::new(CircuitBreakerConfig::default()),
        operation_timeout_config: None,
        control_pool: OnceCell::new(),
        placement_pools: OnceCell::new(),
        dynamic_placement_pools: moka::future::Cache::builder().max_capacity(1_000).build(),
        shard_placement_cache: new_shard_placement_cache(),
    };
    assert!(database_router.control_pool.set(pool).is_ok());
    database_router
}

#[sqlx::test(migrations = "../migrations")]
#[ignore = "requires a PostgreSQL DATABASE_URL for sqlx status tests"]
async fn creation_failure_status_does_not_resolve_unseeded_routes(pool: sqlx::PgPool) {
    let run_id = Uuid::now_v7();
    let dataset_id = Uuid::now_v7();
    let dataset_version_id = Uuid::now_v7();
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
                profile_version_id, profile_hash, aggregation_policy_id,
                aggregation_policy_version, aggregation_policy_hash,
                agent_provider, agent_name, prompt_config_id,
                prompt_config_version, status, error_message, completed_at
            )
            VALUES (
                $1, $2, $3, $4, 'test', 'profile', '1.0.0', 'profile-version',
                'profile-hash', 'aggregation', '1.0.0', 'aggregation-hash',
                'example', 'agent', 'prompt', '1.0.0', 'failed'::run_status,
                'immutable seed mismatch', now()
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
    sqlx::query(
        r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('unseeded', 'VIGILO_TEST_UNSEEDED_DATABASE_URL', 'shard', 'active')
            "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1, 0, 'unseeded', 'active')
            "#,
    )
    .bind(run_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
            INSERT INTO run_creation_placements (
                run_id, database_alias, status, attempt_count,
                expected_case_count, seeded_case_count,
                case_projection_hash, last_error
            )
            VALUES (
                $1, 'unseeded', 'failed', 1,
                1, 0, 'fixture-projection-hash', 'immutable seed mismatch'
            )
            "#,
    )
    .bind(run_id)
    .execute(&pool)
    .await
    .unwrap();
    let database_router = database_router_with_control_pool(pool);

    let status = select_run_status(&database_router, run_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(status.execution_route_count, 0);
    assert_eq!(
        status
            .creation_progress
            .as_ref()
            .map(|progress| progress.failed_placement_count),
        Some(1)
    );
}
