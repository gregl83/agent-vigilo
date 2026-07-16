//! End-to-end multi-database runtime integration coverage.
//!
//! This harness is opt-in because it requires two PostgreSQL databases,
//! RabbitMQ, and a built evaluator WASM artifact. It drives the public `vigilo`
//! binary through create, dispatch, worker processing, finalization,
//! results/export, cancellation, and validation failure paths.

use std::{
    fs,
    io::{
        Read,
        Write,
    },
    net::{
        TcpListener,
        TcpStream,
    },
    path::{
        Path,
        PathBuf,
    },
    process::{
        Command,
        Output,
    },
    thread,
};

use serde_json::{
    Value,
    json,
};
use sqlx::{
    PgPool,
    postgres::PgPoolOptions,
};
use uuid::Uuid;

const PRIMARY_DATABASE_URL_ENV: &str = "DATABASE_URL";
const SHARD_DATABASE_URL_ENV: &str = "VIGILO_TEST_SHARD_001_DATABASE_URL";
const E2E_ENABLED_ENV: &str = "VIGILO_E2E_MULTI_DATABASE";
const PRIMARY_ALIAS: &str = "primary";
const SHARD_ALIAS: &str = "shard_001";
const SHARD_DATABASE_URL_ENV_VALUE: &str = "VIGILO_TEST_SHARD_001_DATABASE_URL";
const SENTIMENT_EVALUATOR_REF: &str = "vigilo/sentiment-basic-en:0.1.0";
const SENTIMENT_WASM_PATH: &str = "target/wasm32-wasip2/release/sentiment_basic_en.wasm";

#[tokio::test]
async fn multi_database_end_to_end_runtime_flow() -> anyhow::Result<()> {
    let Some(config) = IntegrationConfig::from_env() else {
        return Ok(());
    };

    let agent = MockAgent::start()?;
    let primary = connect(&config.primary_url).await?;
    let shard = connect(&config.shard_url).await?;
    migrate(&primary).await?;
    migrate(&shard).await?;
    seed_sentiment_evaluator(&primary).await?;
    configure_shard_placement(&primary).await?;

    let completed_run_id = create_run(
        &config,
        RunCreateInput {
            default_execution_alias: PRIMARY_ALIAS,
            shard_assignment_policy: "spread-active",
            agent_url: &agent.url,
            profile_id: "e2e_completed",
            case_count: 101,
            case_mode: CaseMode::Passing,
            evaluator_ref: SENTIMENT_EVALUATOR_REF,
        },
    )?;

    assert_eq!(
        shard_placement_alias(&primary, completed_run_id, 0).await?,
        PRIMARY_ALIAS
    );
    assert_eq!(
        shard_placement_alias(&primary, completed_run_id, 1).await?,
        SHARD_ALIAS
    );

    run_vigilo_ok(
        &config,
        PRIMARY_ALIAS,
        "spread-active",
        [
            "coordinator",
            "--run-chunk-dispatch-window-size",
            "512",
            "once",
        ],
    )?;
    run_vigilo_ok(&config, PRIMARY_ALIAS, "spread-active", ["worker", "once"])?;
    run_vigilo_ok(&config, PRIMARY_ALIAS, "spread-active", ["worker", "once"])?;
    run_vigilo_ok(
        &config,
        PRIMARY_ALIAS,
        "spread-active",
        ["coordinator", "once"],
    )?;

    let status = run_vigilo_json(
        &config,
        PRIMARY_ALIAS,
        "spread-active",
        ["run", "status", &completed_run_id.to_string()],
    )?;
    assert_eq!(status["data"]["status"].as_str(), Some("completed"));
    assert_eq!(status["data"]["gate_status"].as_str(), Some("pass"));
    assert_eq!(status["data"]["expected_execution_count"], 101);
    assert_eq!(status["data"]["terminal_execution_count"], 101);

    let results = run_vigilo_json(
        &config,
        PRIMARY_ALIAS,
        "spread-active",
        ["run", "results", &completed_run_id.to_string()],
    )?;
    assert_eq!(results["data"]["results"]["execution_count"], 101);
    assert_eq!(
        results["data"]["results"]["status_counts"]["passed"],
        json!(101)
    );

    let export = run_vigilo_output(
        &config,
        PRIMARY_ALIAS,
        "spread-active",
        [
            "run",
            "export",
            &completed_run_id.to_string(),
            "--format",
            "jsonl",
            "--batch-size",
            "25",
        ],
    )?;
    let export_lines = parse_jsonl(&export.stdout)?;
    assert_eq!(
        export_lines
            .iter()
            .filter(|line| line["type"] == "execution")
            .count(),
        101
    );
    assert!(export_lines.iter().any(|line| {
        line["type"] == "execution_route" && line["database_alias"] == PRIMARY_ALIAS
    }));
    assert!(export_lines.iter().any(|line| {
        line["type"] == "execution_route" && line["database_alias"] == SHARD_ALIAS
    }));

    assert_eq!(execution_count(&primary, completed_run_id).await?, 100);
    assert_eq!(execution_count(&shard, completed_run_id).await?, 1);
    assert_eq!(
        outbox_event_count(&primary, completed_run_id, "run.completed").await?,
        1
    );
    assert_eq!(
        outbox_event_count(&shard, completed_run_id, "run.completed").await?,
        0
    );
    assert_eq!(
        outbox_event_count(&shard, completed_run_id, "run.chunk.ready").await?,
        1
    );
    assert_eq!(
        dispatch_cursor_status_count(&primary, completed_run_id, "drained").await?,
        2
    );

    let failing_run_id = create_run(
        &config,
        RunCreateInput {
            default_execution_alias: PRIMARY_ALIAS,
            shard_assignment_policy: "single-default",
            agent_url: &agent.url,
            profile_id: "e2e_agent_failure",
            case_count: 1,
            case_mode: CaseMode::AgentFailure,
            evaluator_ref: SENTIMENT_EVALUATOR_REF,
        },
    )?;
    run_vigilo_ok(
        &config,
        PRIMARY_ALIAS,
        "single-default",
        ["coordinator", "once"],
    )?;
    run_vigilo_ok(&config, PRIMARY_ALIAS, "single-default", ["worker", "once"])?;
    run_vigilo_ok(
        &config,
        PRIMARY_ALIAS,
        "single-default",
        ["coordinator", "once"],
    )?;

    let failed_status = run_vigilo_json(
        &config,
        PRIMARY_ALIAS,
        "single-default",
        ["run", "status", &failing_run_id.to_string()],
    )?;
    assert_eq!(failed_status["data"]["status"].as_str(), Some("completed"));
    assert_eq!(failed_status["data"]["gate_status"].as_str(), Some("fail"));
    assert_eq!(failed_status["data"]["errored_execution_count"], 1);
    assert_eq!(
        execution_status_count(&primary, failing_run_id, "failed").await?,
        1
    );

    let cancel_run_id = create_run(
        &config,
        RunCreateInput {
            default_execution_alias: SHARD_ALIAS,
            shard_assignment_policy: "single-default",
            agent_url: &agent.url,
            profile_id: "e2e_cancelled",
            case_count: 1,
            case_mode: CaseMode::Passing,
            evaluator_ref: SENTIMENT_EVALUATOR_REF,
        },
    )?;
    let cancel_payload = run_vigilo_json(
        &config,
        SHARD_ALIAS,
        "single-default",
        ["run", "cancel", &cancel_run_id.to_string()],
    )?;
    assert_eq!(cancel_payload["meta"]["cancelled"], true);
    assert_eq!(
        run_chunk_status_count(&shard, cancel_run_id, "cancelled").await?,
        1
    );
    assert_eq!(
        outbox_event_count(&primary, cancel_run_id, "run.cancelled").await?,
        1
    );
    assert_eq!(
        outbox_event_count(&shard, cancel_run_id, "run.cancelled").await?,
        0
    );

    let invalid = create_run_output(
        &config,
        RunCreateInput {
            default_execution_alias: PRIMARY_ALIAS,
            shard_assignment_policy: "single-default",
            agent_url: &agent.url,
            profile_id: "e2e_invalid_evaluator",
            case_count: 1,
            case_mode: CaseMode::Passing,
            evaluator_ref: "vigilo/missing:0.0.0",
        },
    )?;
    assert!(
        !invalid.status.success(),
        "run create with an unpublished evaluator should fail"
    );
    assert!(
        String::from_utf8_lossy(&invalid.stderr).contains("unpublished evaluator"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&invalid.stderr)
    );

    Ok(())
}

#[derive(Clone)]
struct IntegrationConfig {
    primary_url: String,
    shard_url: String,
    messaging_url: String,
    mq_namespace: String,
}

impl IntegrationConfig {
    fn from_env() -> Option<Self> {
        if std::env::var(E2E_ENABLED_ENV).ok().as_deref() != Some("1") {
            return None;
        }

        Some(Self {
            primary_url: std::env::var(PRIMARY_DATABASE_URL_ENV).ok()?,
            shard_url: std::env::var(SHARD_DATABASE_URL_ENV).ok()?,
            messaging_url: std::env::var("MESSAGING_URL").ok()?,
            mq_namespace: format!("e2e-{}", Uuid::now_v7()),
        })
    }
}

struct MockAgent {
    url: String,
}

impl MockAgent {
    fn start() -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle_agent_request(stream);
            }
        });

        Ok(Self {
            url: format!("http://{addr}/v1/agent/invoke"),
        })
    }
}

fn handle_agent_request(mut stream: TcpStream) -> anyhow::Result<()> {
    let mut buffer = [0u8; 16 * 1024];
    let read = stream.read(&mut buffer)?;
    let request = String::from_utf8_lossy(&buffer[..read]);

    let (status, body) = if request.contains("force-agent-error") {
        (
            "HTTP/1.1 500 Internal Server Error",
            json!({"error": "forced integration agent failure"}).to_string(),
        )
    } else {
        (
            "HTTP/1.1 200 OK",
            json!({"text": "good reliable response"}).to_string(),
        )
    };

    let response = format!(
        "{status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    Ok(())
}

async fn connect(url: &str) -> anyhow::Result<PgPool> {
    Ok(PgPoolOptions::new().max_connections(5).connect(url).await?)
}

async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("../migrations").run(pool).await?;
    Ok(())
}

async fn seed_sentiment_evaluator(primary: &PgPool) -> anyhow::Result<()> {
    let wasm_bytes = fs::read(SENTIMENT_WASM_PATH).map_err(|err| {
        anyhow::anyhow!(
            "failed to read {}; build it first with `cargo build -p sentiment-basic-en --target wasm32-wasip2 --release --locked`: {}",
            SENTIMENT_WASM_PATH,
            err
        )
    })?;
    let wasm_size_bytes = wasm_bytes.len() as i64;
    let content_hash = blake3::hash(&wasm_bytes).to_hex().to_string();

    sqlx::query(
        r#"
        INSERT INTO evaluators (
            namespace,
            name,
            version,
            content_hash,
            wasm_bytes,
            wasm_size_bytes,
            interface_name,
            interface_version,
            wit_world,
            runtime,
            runtime_version,
            runtime_fingerprint,
            description,
            tags,
            metadata,
            state
        )
        VALUES (
            'vigilo',
            'sentiment-basic-en',
            '0.1.0',
            $1,
            $2,
            $3,
            'evaluator',
            '0.1.0',
            'evaluator-world',
            'wasmtime',
            'integration',
            'integration',
            'Phase 23 integration evaluator',
            '["integration", "sentiment"]'::jsonb,
            '{}'::jsonb,
            'active'::evaluator_state
        )
        ON CONFLICT (namespace, name, version) DO UPDATE
        SET content_hash = EXCLUDED.content_hash,
            wasm_bytes = EXCLUDED.wasm_bytes,
            wasm_size_bytes = EXCLUDED.wasm_size_bytes,
            interface_name = EXCLUDED.interface_name,
            interface_version = EXCLUDED.interface_version,
            wit_world = EXCLUDED.wit_world,
            runtime = EXCLUDED.runtime,
            runtime_version = EXCLUDED.runtime_version,
            runtime_fingerprint = EXCLUDED.runtime_fingerprint,
            description = EXCLUDED.description,
            tags = EXCLUDED.tags,
            metadata = EXCLUDED.metadata,
            state = EXCLUDED.state,
            updated_at = now()
        "#,
    )
    .bind(content_hash)
    .bind(wasm_bytes)
    .bind(wasm_size_bytes)
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
    .bind(SHARD_DATABASE_URL_ENV_VALUE)
    .execute(primary)
    .await?;

    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum CaseMode {
    Passing,
    AgentFailure,
}

fn create_run(config: &IntegrationConfig, input: RunCreateInput<'_>) -> anyhow::Result<Uuid> {
    let output = create_run_output(config, input)?;
    if !output.status.success() {
        anyhow::bail!(
            "run create failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let payload: Value = serde_json::from_slice(&output.stdout)?;
    let run_id = payload["data"]["run_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("run create response did not include data.run_id"))?;

    Ok(Uuid::parse_str(run_id)?)
}

#[derive(Clone, Copy)]
struct RunCreateInput<'a> {
    default_execution_alias: &'a str,
    shard_assignment_policy: &'a str,
    agent_url: &'a str,
    profile_id: &'a str,
    case_count: usize,
    case_mode: CaseMode,
    evaluator_ref: &'a str,
}

fn create_run_output(
    config: &IntegrationConfig,
    input: RunCreateInput<'_>,
) -> anyhow::Result<Output> {
    let test_files = write_run_inputs(
        input.profile_id,
        input.agent_url,
        input.evaluator_ref,
        input.case_count,
        input.case_mode,
    )?;
    run_vigilo_output(
        config,
        input.default_execution_alias,
        input.shard_assignment_policy,
        [
            "run",
            "create",
            "--profile-file",
            path_arg(&test_files.profile)?,
            "--dataset-file",
            path_arg(&test_files.dataset)?,
        ],
    )
}

fn run_vigilo_json<'a>(
    config: &IntegrationConfig,
    default_execution_alias: &str,
    shard_assignment_policy: &str,
    args: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<Value> {
    let output = run_vigilo_ok(
        config,
        default_execution_alias,
        shard_assignment_policy,
        args,
    )?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn run_vigilo_ok<'a>(
    config: &IntegrationConfig,
    default_execution_alias: &str,
    shard_assignment_policy: &str,
    args: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<Output> {
    let output = run_vigilo_output(
        config,
        default_execution_alias,
        shard_assignment_policy,
        args,
    )?;
    if !output.status.success() {
        anyhow::bail!(
            "vigilo command failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(output)
}

fn run_vigilo_output<'a>(
    config: &IntegrationConfig,
    default_execution_alias: &str,
    shard_assignment_policy: &str,
    args: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<Output> {
    let args = args.into_iter().collect::<Vec<_>>();
    Ok(Command::new(env!("CARGO_BIN_EXE_vigilo"))
        .env(PRIMARY_DATABASE_URL_ENV, &config.primary_url)
        .env(SHARD_DATABASE_URL_ENV, &config.shard_url)
        .env("MESSAGING_URL", &config.messaging_url)
        .env("VIGILO_MQ_NAMESPACE", &config.mq_namespace)
        .env(
            "VIGILO_DEFAULT_SHARD_DATABASE_ALIAS",
            default_execution_alias,
        )
        .env("VIGILO_SHARD_ASSIGNMENT_POLICY", shard_assignment_policy)
        .args(["-q", "-f", "json"])
        .args(&args)
        .output()?)
}

struct RunInputFiles {
    profile: PathBuf,
    dataset: PathBuf,
}

fn write_run_inputs(
    profile_id: &str,
    agent_url: &str,
    evaluator_ref: &str,
    case_count: usize,
    case_mode: CaseMode,
) -> anyhow::Result<RunInputFiles> {
    let dir = std::env::temp_dir()
        .join("vigilo-multi-database-e2e")
        .join(Uuid::now_v7().to_string());
    fs::create_dir_all(&dir)?;

    let profile = dir.join("profile.yaml");
    let dataset = dir.join("dataset.yaml");
    fs::write(&profile, profile_yaml(profile_id, agent_url, evaluator_ref))?;
    fs::write(&dataset, dataset_yaml(case_count, case_mode))?;

    Ok(RunInputFiles { profile, dataset })
}

fn path_arg(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("test path is not valid UTF-8: {}", path.display()))
}

fn profile_yaml(profile_id: &str, agent_url: &str, evaluator_ref: &str) -> String {
    format!(
        r#"
profile_id: {profile_id}
profile_version: 1.0.0
description: Phase 23 end-to-end multi-database profile.
defaults:
  max_attempts: 1
  request_timeout_secs: 10
  fail_on_any_blocking_failure: true
  min_execution_score: 0.5
persistence:
  mode: full
  persist_raw_outputs: all
  persist_evaluator_evidence: true
agent:
  provider: integration
  name: mock-agent
  http:
    url: {agent_url}
case_groups:
  - id: sentiment
    description: Integration sentiment cases.
    applies_to:
      task_type: classification
    evaluators:
      - ref: {evaluator_ref}
        dimension: quality
        blocking: true
        weight: 1.0
    aggregation:
      dimensions:
        quality:
          method: min_score
          blocking: true
          weight: 1.0
"#
    )
}

fn dataset_yaml(case_count: usize, case_mode: CaseMode) -> String {
    let dataset_id = Uuid::now_v7();
    let mut raw = format!("dataset_id: {dataset_id}\ndataset_version: 1.0.0\ncases:\n");

    for ordinal in 0..case_count {
        let case_id = Uuid::now_v7();
        let prompt = match case_mode {
            CaseMode::Passing => format!("case {ordinal}: this is good and reliable"),
            CaseMode::AgentFailure => "force-agent-error".to_string(),
        };
        raw.push_str(&format!(
            r#"  - id: {case_id}
    task_type: classification
    input:
      user_message: "{prompt}"
    expected:
      label: positive
    tags: [integration]
    metadata:
      ordinal: {ordinal}
"#
        ));
    }

    raw
}

fn parse_jsonl(bytes: &[u8]) -> anyhow::Result<Vec<Value>> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
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

async fn execution_count(db: &PgPool, run_id: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM executions
        WHERE run_id = $1::uuid
        "#,
    )
    .bind(run_id)
    .fetch_one(db)
    .await?)
}

async fn execution_status_count(db: &PgPool, run_id: Uuid, status: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM executions
        WHERE run_id = $1::uuid
          AND status = $2
        "#,
    )
    .bind(run_id)
    .bind(status)
    .fetch_one(db)
    .await?)
}

async fn run_chunk_status_count(db: &PgPool, run_id: Uuid, status: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM run_chunks
        WHERE run_id = $1::uuid
          AND status = $2
        "#,
    )
    .bind(run_id)
    .bind(status)
    .fetch_one(db)
    .await?)
}

async fn dispatch_cursor_status_count(
    db: &PgPool,
    run_id: Uuid,
    status: &str,
) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM run_shard_dispatch_cursors
        WHERE run_id = $1::uuid
          AND status = $2
        "#,
    )
    .bind(run_id)
    .bind(status)
    .fetch_one(db)
    .await?)
}

async fn outbox_event_count(db: &PgPool, run_id: Uuid, event_type: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::bigint
        FROM outbox_events
        WHERE aggregate_id = $1::uuid
          AND event_type = $2
        "#,
    )
    .bind(run_id)
    .bind(event_type)
    .fetch_one(db)
    .await?)
}
