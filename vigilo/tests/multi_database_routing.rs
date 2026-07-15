//! Black-box multi-database routing integration coverage.
//!
//! This test is opt-in because it requires two independent PostgreSQL
//! databases. It drives the `vigilo` binary for public CLI behavior and uses
//! SQL only for schema setup, seed data, and persistence assertions.

use std::{
    fs,
    path::{
        Path,
        PathBuf,
    },
    process::Command,
};

use serde_json::Value;
use sqlx::{
    PgPool,
    postgres::PgPoolOptions,
};
use uuid::Uuid;

const PRIMARY_DATABASE_URL_ENV: &str = "DATABASE_URL";
const SHARD_DATABASE_URL_ENV: &str = "VIGILO_TEST_SHARD_001_DATABASE_URL";
const PRIMARY_ALIAS: &str = "primary";
const SHARD_ALIAS: &str = "shard_001";
const TEST_MESSAGING_URL: &str = "amqp://guest:guest@localhost:5672";

#[tokio::test]
async fn multi_database_routing_flow() -> anyhow::Result<()> {
    let Some(config) = IntegrationConfig::from_env() else {
        return Ok(());
    };

    let primary = connect(&config.primary_url).await?;
    let shard = connect(&config.shard_url).await?;
    migrate(&primary).await?;
    migrate(&shard).await?;
    seed_evaluator(&primary).await?;
    configure_shard_placement(&primary).await?;

    let default_run_id = create_run(&config, PRIMARY_ALIAS, 1).await?;
    assert_eq!(run_chunk_count(&primary, default_run_id).await?, 1);
    assert_eq!(run_chunk_count(&shard, default_run_id).await?, 0);
    assert_eq!(
        shard_placement_alias(&primary, default_run_id, 0).await?,
        PRIMARY_ALIAS
    );
    let default_route = run_vigilo(
        &config,
        PRIMARY_ALIAS,
        ["shard", "route", &default_run_id.to_string(), "0"],
    )?;
    assert_eq!(
        default_route["data"]["database_alias"].as_str(),
        Some(PRIMARY_ALIAS)
    );
    assert_eq!(default_route["data"]["dispatchable"], true);
    assert_eq!(default_route["data"]["database_url_env_resolved"], true);
    assert_eq!(
        dispatch_cursor_count(&primary, default_run_id).await?,
        1,
        "dispatch cursors are control-plane state"
    );
    assert_eq!(dispatch_cursor_count(&shard, default_run_id).await?, 0);

    let routed_run_id = create_run(&config, SHARD_ALIAS, 101).await?;
    assert_eq!(run_chunk_count(&primary, routed_run_id).await?, 0);
    assert_eq!(run_chunk_count(&shard, routed_run_id).await?, 2);
    assert_eq!(
        shard_placement_alias(&primary, routed_run_id, 0).await?,
        SHARD_ALIAS
    );
    assert_eq!(
        shard_placement_alias(&primary, routed_run_id, 1).await?,
        SHARD_ALIAS
    );
    let routed_route = run_vigilo(
        &config,
        SHARD_ALIAS,
        ["shard", "route", &routed_run_id.to_string(), "1"],
    )?;
    assert_eq!(
        routed_route["data"]["database_alias"].as_str(),
        Some(SHARD_ALIAS)
    );
    assert_eq!(
        routed_route["data"]["database_url_env"].as_str(),
        Some(SHARD_DATABASE_URL_ENV)
    );
    assert_eq!(routed_route["data"]["dispatchable"], true);
    assert_eq!(dispatch_cursor_count(&primary, routed_run_id).await?, 2);
    assert_eq!(dispatch_cursor_count(&shard, routed_run_id).await?, 0);

    let move_payload = run_vigilo(
        &config,
        PRIMARY_ALIAS,
        [
            "shard",
            "move",
            &routed_run_id.to_string(),
            "0",
            "--alias",
            PRIMARY_ALIAS,
        ],
    )?;
    assert_eq!(move_payload["meta"]["moved"], true);
    assert_eq!(
        move_payload["data"]["target_database_alias"].as_str(),
        Some(PRIMARY_ALIAS)
    );
    assert_eq!(
        shard_placement_alias(&primary, routed_run_id, 0).await?,
        PRIMARY_ALIAS
    );
    let moved_route = run_vigilo(
        &config,
        PRIMARY_ALIAS,
        ["shard", "route", &routed_run_id.to_string(), "0"],
    )?;
    assert_eq!(
        moved_route["data"]["database_alias"].as_str(),
        Some(PRIMARY_ALIAS)
    );
    assert_eq!(moved_route["data"]["routing_decision"], "dispatchable");
    assert_eq!(run_shard_chunk_count(&primary, routed_run_id, 0).await?, 1);
    assert_eq!(run_shard_chunk_count(&shard, routed_run_id, 0).await?, 1);

    Ok(())
}

struct IntegrationConfig {
    primary_url: String,
    shard_url: String,
}

impl IntegrationConfig {
    fn from_env() -> Option<Self> {
        Some(Self {
            primary_url: std::env::var(PRIMARY_DATABASE_URL_ENV).ok()?,
            shard_url: std::env::var(SHARD_DATABASE_URL_ENV).ok()?,
        })
    }
}

async fn connect(url: &str) -> anyhow::Result<PgPool> {
    Ok(PgPoolOptions::new().max_connections(5).connect(url).await?)
}

async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("../migrations").run(pool).await?;
    Ok(())
}

async fn seed_evaluator(primary: &PgPool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO evaluators (
            namespace,
            name,
            version,
            content_hash,
            wasm_bytes,
            wasm_size_bytes,
            runtime,
            runtime_version,
            runtime_fingerprint,
            state
        )
        VALUES (
            'test',
            'json-schema',
            '1.0.0',
            'multi-database-routing-json-schema',
            decode('', 'hex'),
            0,
            'wasmtime',
            'integration',
            'integration',
            'active'::evaluator_state
        )
        ON CONFLICT (namespace, name, version) DO UPDATE
        SET content_hash = EXCLUDED.content_hash,
            wasm_bytes = EXCLUDED.wasm_bytes,
            wasm_size_bytes = EXCLUDED.wasm_size_bytes,
            runtime = EXCLUDED.runtime,
            runtime_version = EXCLUDED.runtime_version,
            runtime_fingerprint = EXCLUDED.runtime_fingerprint,
            state = EXCLUDED.state,
            updated_at = now()
        "#,
    )
    .execute(primary)
    .await?;

    Ok(())
}

async fn configure_shard_placement(primary: &PgPool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO database_placements (alias, database_url_env, role, status)
        VALUES ($1, $2, 'shard', 'active')
        ON CONFLICT (alias) DO UPDATE
        SET database_url_env = EXCLUDED.database_url_env,
            role = EXCLUDED.role,
            status = EXCLUDED.status,
            updated_at = now()
        "#,
    )
    .bind(SHARD_ALIAS)
    .bind(SHARD_DATABASE_URL_ENV)
    .execute(primary)
    .await?;

    Ok(())
}

async fn create_run(
    config: &IntegrationConfig,
    default_execution_alias: &str,
    case_count: usize,
) -> anyhow::Result<Uuid> {
    let test_files = write_run_inputs(case_count)?;
    let payload = run_vigilo(
        config,
        default_execution_alias,
        [
            "run",
            "create",
            "--profile-file",
            path_arg(&test_files.profile)?,
            "--dataset-file",
            path_arg(&test_files.dataset)?,
        ],
    )?;
    let run_id = payload["data"]["run_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("run create response did not include data.run_id"))?;

    Ok(Uuid::parse_str(run_id)?)
}

fn run_vigilo<'a>(
    config: &IntegrationConfig,
    default_execution_alias: &str,
    args: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<Value> {
    let output = Command::new(env!("CARGO_BIN_EXE_vigilo"))
        .env(PRIMARY_DATABASE_URL_ENV, &config.primary_url)
        .env(SHARD_DATABASE_URL_ENV, &config.shard_url)
        .env("MESSAGING_URL", TEST_MESSAGING_URL)
        .env(
            "VIGILO_DEFAULT_SHARD_DATABASE_ALIAS",
            default_execution_alias,
        )
        .args(["-q", "-f", "json"])
        .args(args)
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "vigilo command failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(serde_json::from_slice(&output.stdout)?)
}

struct RunInputFiles {
    profile: PathBuf,
    dataset: PathBuf,
}

fn write_run_inputs(case_count: usize) -> anyhow::Result<RunInputFiles> {
    let dir = std::env::temp_dir()
        .join("vigilo-multi-database-routing")
        .join(Uuid::now_v7().to_string());
    fs::create_dir_all(&dir)?;

    let profile = dir.join("profile.yaml");
    let dataset = dir.join("dataset.yaml");
    fs::write(&profile, profile_yaml())?;
    fs::write(&dataset, dataset_yaml(case_count))?;

    Ok(RunInputFiles { profile, dataset })
}

fn path_arg(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("test path is not valid UTF-8: {}", path.display()))
}

fn profile_yaml() -> &'static str {
    r#"
profile_id: multi_database_routing
profile_version: 1.0.0
description: Integration profile for multi-database routing.
defaults:
  max_attempts: 1
  request_timeout_secs: 30
  fail_on_any_blocking_failure: true
  min_execution_score: 0.5
persistence:
  mode: full
  persist_raw_outputs: all
  persist_evaluator_evidence: true
agent:
  provider: example
  name: integration-agent
  http:
    url: http://127.0.0.1:8787/v1/agent/invoke
case_groups:
  - id: classification
    description: Integration classification cases.
    applies_to:
      task_type: classification
    evaluators:
      - ref: test/json-schema:1.0.0
        dimension: format
        blocking: true
        weight: 1.0
    aggregation:
      dimensions:
        format:
          method: min_score
          blocking: true
          weight: 1.0
"#
}

fn dataset_yaml(case_count: usize) -> String {
    let dataset_id = Uuid::now_v7();
    let mut raw = format!("dataset_id: {dataset_id}\ndataset_version: 1.0.0\ncases:\n");

    for ordinal in 0..case_count {
        let case_id = Uuid::now_v7();
        raw.push_str(&format!(
            r#"  - id: {case_id}
    task_type: classification
    input:
      prompt: "case {ordinal}"
    expected:
      label: ok
    tags: [integration]
    metadata:
      ordinal: {ordinal}
"#
        ));
    }

    raw
}

async fn run_chunk_count(db: &PgPool, run_id: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM run_chunks
        WHERE run_id = $1::uuid
        "#,
    )
    .bind(run_id)
    .fetch_one(db)
    .await?)
}

async fn run_shard_chunk_count(db: &PgPool, run_id: Uuid, run_shard: i16) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM run_chunks
        WHERE run_id = $1::uuid
          AND run_shard = $2
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .fetch_one(db)
    .await?)
}

async fn dispatch_cursor_count(db: &PgPool, run_id: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM run_shard_dispatch_cursors
        WHERE run_id = $1::uuid
        "#,
    )
    .bind(run_id)
    .fetch_one(db)
    .await?)
}

async fn shard_placement_alias(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<String> {
    Ok(sqlx::query_scalar::<_, String>(
        r#"
        SELECT database_alias
        FROM shard_placements
        WHERE run_id = $1::uuid
          AND run_shard = $2
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .fetch_one(db)
    .await?)
}
