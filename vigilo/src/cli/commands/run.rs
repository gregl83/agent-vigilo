//! Run management commands.
//!
//! This module owns CLI flows for creating, validating, and watching runs. It
//! translates profile+dataset inputs into normalized persistence drafts and run
//! orchestration metadata.

use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
};

use async_trait::async_trait;
use blake3::Hasher;
use clap::{
    Args,
    Subcommand,
    ValueEnum,
};
use serde_json::{
    Value,
    json,
};
use tokio::time::{
    Duration,
    Instant,
    sleep,
};
use tracing::info;
use uuid::Uuid;

use super::{
    Executable,
    args::parsers::parse_filepath,
};
use crate::{
    context::Context,
    contracts::run::{
        RunDataset,
        RunProfile,
    },
    db::{
        tables::runs,
        workflows::{
            run_cancel,
            run_create,
            run_profile_validation,
        },
    },
    models::{
        case_blob::CaseBlobDraft,
        dataset_version_case::DatasetVersionCaseDraft,
        evaluator_result::EvaluatorResult,
        execution::Execution,
        execution_aggregate::ExecutionAggregate,
        execution_attempt::ExecutionAttempt,
        run::{
            Run,
            RunDraft,
        },
        run_chunk::RunChunkDraft,
    },
};

const DEFAULT_CHUNK_SIZE: usize = 100;
const DEFAULT_WATCH_INTERVAL_SECONDS: u64 = 5;
const MAX_WATCH_INTERVAL_SECONDS: u64 = 3_600;
const MAX_WATCH_TIMEOUT_SECONDS: u64 = 604_800;
const EXPORT_EXECUTION_BATCH_SIZE: i64 = 250;
const MAX_EXPORT_BATCH_SIZE: i64 = 10_000;

/// Parsed and typed run inputs loaded from CLI sources.
///
/// Keeps canonical payloads for hashing/snapshot fields plus typed contracts
/// for validation and planning, avoiding repeated parse work.
struct ParsedRunInputs {
    profile_payload: Value,
    dataset_payload: Value,
    profile: RunProfile,
    dataset: RunDataset,
}

/// Reads a YAML/JSON payload from either inline text or a file path.
///
/// Exactly one source must be provided.
fn read_inline_or_file(
    inline: Option<String>,
    file: Option<PathBuf>,
    field: &str,
) -> anyhow::Result<String> {
    match (inline, file) {
        (Some(raw), None) => Ok(raw),
        (None, Some(path)) => fs::read_to_string(path)
            .map_err(|err| anyhow::anyhow!("failed to read {} file: {}", field, err)),
        _ => anyhow::bail!(
            "exactly one of --{} or --{}-file must be provided",
            field,
            field
        ),
    }
}

/// Parses YAML or JSON text into a generic JSON value.
fn parse_structured_payload(raw: &str, field: &str) -> anyhow::Result<Value> {
    serde_yaml::from_str::<Value>(raw)
        .map_err(|err| anyhow::anyhow!("invalid {} payload (yaml/json expected): {}", field, err))
}

/// Converts JSON objects into deterministic key order recursively.
///
/// This ensures stable hashing and snapshot serialization.
fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = BTreeMap::new();
            for (key, val) in map {
                sorted.insert(key.clone(), canonical_json(val));
            }

            let mut out = serde_json::Map::new();
            for (key, val) in sorted {
                out.insert(key, val);
            }

            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

/// Hashes a JSON value after serialization using BLAKE3.
fn hash_json(value: &Value) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(value)?;
    let mut hasher = Hasher::new();
    hasher.update(&bytes);
    Ok(hasher.finalize().to_hex().to_string())
}

/// Builds a deterministic UUID from canonical JSON bytes.
fn uuid_from_json(value: &Value) -> anyhow::Result<Uuid> {
    let bytes = serde_json::to_vec(value)?;
    let mut hasher = Hasher::new();
    hasher.update(&bytes);
    let hash = hasher.finalize();

    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&hash.as_bytes()[..16]);
    uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x80;
    uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;

    Ok(Uuid::from_bytes(uuid_bytes))
}

/// Sorts tags lexicographically for deterministic storage and hashing.
fn canonical_tags(tags: &[String]) -> Value {
    let mut ordered = tags.to_vec();
    ordered.sort();
    Value::Array(ordered.into_iter().map(Value::String).collect())
}

/// Builds case blob and dataset membership drafts from a parsed run dataset.
///
/// Returns:
/// - case blob rows keyed by `case_hash`
/// - dataset-version membership rows preserving case order
fn build_case_plans(
    dataset: &RunDataset,
) -> anyhow::Result<(Vec<CaseBlobDraft>, Vec<DatasetVersionCaseDraft>)> {
    let mut case_blobs = Vec::with_capacity(dataset.cases.len());
    let mut dataset_cases = Vec::with_capacity(dataset.cases.len());

    for (idx, case) in dataset.cases.iter().enumerate() {
        let expected_output = canonical_json(case.expected.as_ref().unwrap_or(&Value::Null));
        let metadata = canonical_json(&serde_json::to_value(&case.metadata)?);
        let input_payload = canonical_json(&case.input);
        let context_payload = canonical_json(case.context.as_ref().unwrap_or(&Value::Null));
        let tags = canonical_tags(&case.tags);

        let blob_payload = json!({
            "task_type": case.task_type.clone(),
            "input": input_payload,
            "expected_output": expected_output,
            "context": context_payload,
            "tags": tags,
            "metadata": metadata,
        });

        let case_hash = hash_json(&blob_payload)?;

        case_blobs.push(CaseBlobDraft {
            case_hash: case_hash.clone(),
            task_type: case.task_type.clone(),
            input_payload: blob_payload["input"].clone(),
            expected_output: blob_payload["expected_output"].clone(),
            context_payload: blob_payload["context"].clone(),
            tags: blob_payload["tags"].clone(),
            metadata: blob_payload["metadata"].clone(),
        });

        dataset_cases.push(DatasetVersionCaseDraft {
            case_id: case.id.clone(),
            case_ordinal: idx as i32,
            case_hash,
        });
    }

    Ok((case_blobs, dataset_cases))
}

/// Computes a deterministic dataset version id from logical membership.
fn compute_dataset_version_id(
    dataset: &RunDataset,
    dataset_cases: &[DatasetVersionCaseDraft],
) -> anyhow::Result<Uuid> {
    let membership = dataset_cases
        .iter()
        .map(|c| {
            json!({
                "case_id": c.case_id,
                "case_ordinal": c.case_ordinal,
                "case_hash": c.case_hash,
            })
        })
        .collect::<Vec<_>>();

    uuid_from_json(&json!({
        "dataset_id": dataset.dataset_id,
        "dataset_version": dataset.dataset_version,
        "membership": membership,
    }))
}

/// Computes the hash used to version aggregation behavior from profile groups.
fn compute_aggregation_policy_hash(profile: &RunProfile) -> anyhow::Result<String> {
    let groups = profile
        .case_groups
        .iter()
        .map(|group| {
            json!({
                "id": group.id,
                "aggregation": group.aggregation,
            })
        })
        .collect::<Vec<_>>();

    hash_json(&json!({ "case_groups": groups }))
}

/// Builds run chunk drafts from total case count and requested chunk size.
fn build_chunks(total_cases: usize, chunk_size: usize) -> Vec<RunChunkDraft> {
    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < total_cases {
        let end = (start + chunk_size).min(total_cases);
        chunks.push(RunChunkDraft {
            chunk_id: Uuid::now_v7(),
            profile_group_id: "default".to_string(),
            ordinal_start: start as i32,
            ordinal_end: end as i32,
        });
        start = end;
    }

    chunks
}

/// Loads and validates profile/dataset input sources into typed contracts.
fn load_run_inputs(
    profile: Option<String>,
    profile_file: Option<PathBuf>,
    dataset: Option<String>,
    dataset_file: Option<PathBuf>,
) -> anyhow::Result<ParsedRunInputs> {
    let profile_raw = read_inline_or_file(profile, profile_file, "profile")?;
    let dataset_raw = read_inline_or_file(dataset, dataset_file, "dataset")?;

    let profile_payload = parse_structured_payload(&profile_raw, "profile")?;
    let dataset_payload = parse_structured_payload(&dataset_raw, "dataset")?;

    let profile: RunProfile = serde_yaml::from_str(&profile_raw)
        .map_err(|err| anyhow::anyhow!("invalid profile schema: {}", err))?;
    let dataset: RunDataset = serde_yaml::from_str(&dataset_raw)
        .map_err(|err| anyhow::anyhow!("invalid dataset schema: {}", err))?;

    Ok(ParsedRunInputs {
        profile_payload,
        dataset_payload,
        profile,
        dataset,
    })
}

/// Implements `vigilo run create`.
///
/// This flow validates executability, creates case/dataset/run drafts, stores
/// durable run work in one transaction, and emits a machine-readable summary.
/// Coordinators publish chunk-ready events after they mark the run running.
async fn handle_create(
    context: Context,
    profile: Option<String>,
    profile_file: Option<PathBuf>,
    dataset: Option<String>,
    dataset_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    let db = context.db().await?;
    let out = context.out().await?;

    let parsed = load_run_inputs(profile, profile_file, dataset, dataset_file)?;
    let profile_payload = canonical_json(&parsed.profile_payload);
    let dataset_payload = canonical_json(&parsed.dataset_payload);

    if parsed.dataset.cases.is_empty() {
        anyhow::bail!("dataset must include at least one case");
    }

    let executability = run_profile_validation::validate_profile_executability(
        db,
        &parsed.profile,
        &parsed.dataset,
    )
    .await?;

    let (case_blobs, dataset_cases) = build_case_plans(&parsed.dataset)?;
    let dataset_version_id = compute_dataset_version_id(&parsed.dataset, &dataset_cases)?;
    let profile_hash = hash_json(&profile_payload)?;
    let dataset_hash = hash_json(&dataset_payload)?;
    let aggregation_policy_hash = compute_aggregation_policy_hash(&parsed.profile)?;
    let profile_version_id = format!(
        "{}/{}",
        parsed.profile.profile_id, parsed.profile.profile_version
    );

    let chunk_size = DEFAULT_CHUNK_SIZE;
    let chunks = build_chunks(dataset_cases.len(), chunk_size);
    let run_id = Uuid::now_v7();
    let run_key = run_id.to_string();
    let agent = &parsed.profile.agent;

    let snapshot = json!({
        "profile": profile_payload,
        "agent": agent,
        "dataset_ref": {
            "dataset_id": parsed.dataset.dataset_id,
            "dataset_version": parsed.dataset.dataset_version,
            "dataset_version_id": dataset_version_id,
            "dataset_hash": dataset_hash,
            "case_count": dataset_cases.len(),
        },
        "dataset_version_id": dataset_version_id,
        "profile_version_id": profile_version_id,
        "profile_hash": profile_hash,
        "dataset_hash": dataset_hash,
        "aggregation_policy_hash": aggregation_policy_hash,
        "chunk_size": chunk_size,
        "executability": executability,
    });

    let run_draft = RunDraft {
        run_key: run_key.clone(),
        name: None,
        description: None,
        dataset_id: parsed.dataset.dataset_id,
        dataset_version: parsed
            .dataset
            .dataset_version
            .clone()
            .unwrap_or_else(|| dataset_version_id.to_string()),
        dataset_version_id,
        evaluation_profile_id: parsed.profile.profile_id.clone(),
        evaluation_profile_version: parsed.profile.profile_version.clone(),
        profile_version_id: profile_version_id.clone(),
        profile_hash: profile_hash.clone(),
        aggregation_policy_id: "profile_case_group_aggregation".to_string(),
        aggregation_policy_version: "v3".to_string(),
        aggregation_policy_hash: aggregation_policy_hash.clone(),
        agent_provider: agent.provider.clone(),
        agent_name: agent.name.clone(),
        agent_version: agent.version.clone(),
        prompt_config_id: agent
            .prompt_config_id
            .clone()
            .unwrap_or_else(|| "default".to_string()),
        prompt_config_version: agent
            .prompt_config_version
            .clone()
            .unwrap_or_else(|| "v1".to_string()),
        config_snapshot: snapshot,
        expected_execution_count: dataset_cases.len() as i32,
    };

    let mut tx = db.begin().await?;

    run_create::bulk_insert_case_blobs(&mut tx, &case_blobs).await?;
    run_create::upsert_dataset_version(
        &mut tx,
        dataset_version_id,
        run_draft.dataset_id,
        &run_draft.dataset_version,
    )
    .await?;
    run_create::bulk_insert_dataset_membership(&mut tx, dataset_version_id, &dataset_cases).await?;
    run_create::insert_run_create(&mut tx, run_id, &run_draft).await?;
    run_create::bulk_insert_run_chunks(&mut tx, run_id, dataset_version_id, &chunks).await?;

    tx.commit().await?;

    let payload = json!({
        "data": {
            "run_id": run_id,
            "run_key": run_key,
            "dataset_version_id": dataset_version_id,
            "profile_version_id": profile_version_id,
            "profile_hash": profile_hash,
            "dataset_hash": dataset_hash,
            "aggregation_policy_hash": aggregation_policy_hash,
            "status": "pending",
        },
        "meta": {
            "case_count": dataset_cases.len(),
            "chunk_count": chunks.len(),
            "chunk_size": chunk_size,
            "expected_evaluator_executions": executability.expected_evaluator_execution_count,
            "resolved_evaluator_refs": executability.runnable_evaluator_ref_count,
        }
    });

    out.write_value(&payload)?;
    Ok(())
}

/// Implements `vigilo run test`.
///
/// This command performs schema + executability validation and echoes parsed
/// contracts for local inspection without creating persistence records.
async fn handle_test(
    context: Context,
    profile: Option<String>,
    profile_file: Option<PathBuf>,
    dataset: Option<String>,
    dataset_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    let db = context.db().await?;
    let out = context.out().await?;
    let parsed = load_run_inputs(profile, profile_file, dataset, dataset_file)?;
    let executability = run_profile_validation::validate_profile_executability(
        db,
        &parsed.profile,
        &parsed.dataset,
    )
    .await?;

    let payload = json!({
        "data": {
            "profile": parsed.profile,
            "dataset": parsed.dataset,
        },
        "meta": {
            "profile_case_groups": parsed.profile.case_groups.len(),
            "dataset_cases": parsed.dataset.cases.len(),
            "executability": executability,
            "sources": {
                "profile": if parsed.profile_payload.is_object() || parsed.profile_payload.is_array() { "structured" } else { "scalar" },
                "dataset": if parsed.dataset_payload.is_object() || parsed.dataset_payload.is_array() { "structured" } else { "scalar" },
            }
        }
    });

    out.write_value(&payload)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunWatchSnapshotKey {
    status: String,
    gate_status: String,
    expected_execution_count: i32,
    terminal_execution_count: i32,
    passed_execution_count: i32,
    failed_execution_count: i32,
    errored_execution_count: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum RunExportFormat {
    Json,
    Jsonl,
}

impl From<&Run> for RunWatchSnapshotKey {
    fn from(run: &Run) -> Self {
        Self {
            status: run.status.clone(),
            gate_status: run.gate_status.clone(),
            expected_execution_count: run.expected_execution_count,
            terminal_execution_count: run.terminal_execution_count,
            passed_execution_count: run.passed_execution_count,
            failed_execution_count: run.failed_execution_count,
            errored_execution_count: run.errored_execution_count,
        }
    }
}

fn parse_run_id(raw: &str) -> anyhow::Result<Uuid> {
    Uuid::parse_str(raw).map_err(|err| anyhow::anyhow!("invalid run_id '{}': {}", raw, err))
}

fn is_terminal_run_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

fn run_terminal_failure_reason(run: &Run) -> Option<String> {
    match run.status.as_str() {
        "failed" => Some(format!(
            "run '{}' failed before producing a passing gate",
            run.id
        )),
        "cancelled" => Some(format!(
            "run '{}' was cancelled before producing a passing gate",
            run.id
        )),
        _ => None,
    }
}

fn run_gate_failure_reason(run: &Run) -> Option<String> {
    match (run.status.as_str(), run.gate_status.as_str()) {
        ("completed", "pass") => None,
        ("completed", "fail") => Some(format!("run '{}' completed with gate_status=fail", run.id)),
        ("completed", other) => Some(format!(
            "run '{}' completed with unexpected gate_status={}",
            run.id, other
        )),
        _ => None,
    }
}

fn run_watch_payload(run: &Run, terminal: bool) -> Value {
    json!({
        "data": {
            "run_id": run.id,
            "run_key": run.run_key,
            "status": run.status,
            "gate_status": run.gate_status,
            "expected_execution_count": run.expected_execution_count,
            "terminal_execution_count": run.terminal_execution_count,
            "passed_execution_count": run.passed_execution_count,
            "failed_execution_count": run.failed_execution_count,
            "errored_execution_count": run.errored_execution_count,
            "summary": run.summary,
            "error_message": run.error_message,
            "created_at": run.created_at,
            "started_at": run.started_at,
            "dispatched_at": run.dispatched_at,
            "finalized_at": run.finalized_at,
            "completed_at": run.completed_at,
            "updated_at": run.updated_at,
        },
        "meta": {
            "terminal": terminal,
            "gate_passed": run.status == "completed" && run.gate_status == "pass",
        }
    })
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct RunResultsSummary {
    execution_count: i64,
    aggregate_count: i64,
    passed_execution_count: i64,
    failed_execution_count: i64,
    error_execution_count: i64,
    skipped_execution_count: i64,
    missing_aggregate_count: i64,
    evaluator_result_count: i64,
    blocking_failure_count: i64,
    average_score: Option<f64>,
    min_score: Option<f64>,
    max_score: Option<f64>,
}

async fn select_existing_run(db: &sqlx::PgPool, run_id: Uuid) -> anyhow::Result<Run> {
    runs::select_run_by_id(db, run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run '{}' was not found", run_id))
}

async fn select_run_results_summary(
    db: &sqlx::PgPool,
    run_id: Uuid,
) -> anyhow::Result<RunResultsSummary> {
    let summary = sqlx::query_as::<_, RunResultsSummary>(
        r#"
        SELECT
            COUNT(e.id)::bigint AS execution_count,
            COUNT(ea.execution_id)::bigint AS aggregate_count,
            COUNT(*) FILTER (WHERE ea.overall_status = 'passed'::evaluation_status)::bigint AS passed_execution_count,
            COUNT(*) FILTER (WHERE ea.overall_status = 'failed'::evaluation_status)::bigint AS failed_execution_count,
            COUNT(*) FILTER (WHERE ea.overall_status = 'error'::evaluation_status)::bigint AS error_execution_count,
            COUNT(*) FILTER (WHERE ea.overall_status = 'skipped'::evaluation_status)::bigint AS skipped_execution_count,
            COUNT(e.id) FILTER (WHERE ea.execution_id IS NULL)::bigint AS missing_aggregate_count,
            COALESCE(SUM(ea.evaluator_result_count), 0)::bigint AS evaluator_result_count,
            COALESCE(SUM(
                CASE
                    WHEN ea.blocking_failures IS NULL THEN 0
                    ELSE jsonb_array_length(ea.blocking_failures)
                END
            ), 0)::bigint AS blocking_failure_count,
            AVG(ea.aggregate_score) AS average_score,
            MIN(ea.aggregate_score) AS min_score,
            MAX(ea.aggregate_score) AS max_score
        FROM executions e
        LEFT JOIN execution_aggregates ea
          ON ea.run_id = e.run_id
         AND ea.execution_id = e.id
        WHERE e.run_id = $1::uuid
        "#,
    )
    .bind(run_id)
    .fetch_one(db)
    .await?;

    Ok(summary)
}

fn run_results_payload(run: &Run, summary: &RunResultsSummary) -> Value {
    json!({
        "data": {
            "run": {
                "run_id": run.id,
                "run_key": run.run_key,
                "status": run.status,
                "gate_status": run.gate_status,
                "expected_execution_count": run.expected_execution_count,
                "terminal_execution_count": run.terminal_execution_count,
                "passed_execution_count": run.passed_execution_count,
                "failed_execution_count": run.failed_execution_count,
                "errored_execution_count": run.errored_execution_count,
                "summary": run.summary,
                "error_message": run.error_message,
                "created_at": run.created_at,
                "completed_at": run.completed_at,
                "updated_at": run.updated_at,
            },
            "results": {
                "execution_count": summary.execution_count,
                "aggregate_count": summary.aggregate_count,
                "missing_aggregate_count": summary.missing_aggregate_count,
                "status_counts": {
                    "passed": summary.passed_execution_count,
                    "failed": summary.failed_execution_count,
                    "error": summary.error_execution_count,
                    "skipped": summary.skipped_execution_count,
                },
                "score": {
                    "average": summary.average_score,
                    "min": summary.min_score,
                    "max": summary.max_score,
                },
                "evaluator_result_count": summary.evaluator_result_count,
                "blocking_failure_count": summary.blocking_failure_count,
            },
        },
        "meta": {
            "summary_only": true,
        }
    })
}

#[derive(Debug)]
struct RunExportBatch {
    executions: Vec<Execution>,
    attempts: Vec<ExecutionAttempt>,
    aggregates: Vec<ExecutionAggregate>,
    evaluator_results: Vec<EvaluatorResult>,
}

async fn select_execution_batch_by_run_id(
    db: &sqlx::PgPool,
    run_id: Uuid,
    after_execution_id: Option<Uuid>,
    limit: i64,
) -> anyhow::Result<Vec<Execution>> {
    let executions = sqlx::query_as::<_, Execution>(
        r#"
        SELECT
            id,
            run_id,
            case_id,
            task_type,
            tags,
            input_payload,
            expected_output,
            case_metadata,
            evaluation_profile_id,
            evaluation_profile_version,
            evaluator_manifest,
            expected_evaluator_count,
            status::text as status,
            current_attempt_no,
            current_attempt_id,
            last_error_message,
            created_at,
            started_at,
            completed_at,
            updated_at
        FROM executions
        WHERE run_id = $1::uuid
          AND ($2::uuid IS NULL OR id > $2::uuid)
        ORDER BY id
        LIMIT $3
        "#,
    )
    .bind(run_id)
    .bind(after_execution_id)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(executions)
}

async fn select_run_export_batch_for_executions(
    db: &sqlx::PgPool,
    run_id: Uuid,
    executions: Vec<Execution>,
) -> anyhow::Result<RunExportBatch> {
    if executions.is_empty() {
        return Ok(RunExportBatch {
            executions,
            attempts: Vec::new(),
            aggregates: Vec::new(),
            evaluator_results: Vec::new(),
        });
    }

    let execution_ids = executions
        .iter()
        .map(|execution| execution.id)
        .collect::<Vec<_>>();

    let attempts = sqlx::query_as::<_, ExecutionAttempt>(
        r#"
        SELECT
            id,
            execution_id,
            run_id,
            attempt_no,
            status::text as status,
            worker_id,
            worker_host,
            queue_message_id,
            leased_until::text as leased_until,
            heartbeat_at::text as heartbeat_at,
            request_artifact_uri,
            response_artifact_uri,
            agent_latency_ms,
            evaluator_latency_ms,
            total_latency_ms,
            token_usage,
            outcome_summary,
            error_message,
            created_at,
            started_at,
            completed_at,
            updated_at
        FROM execution_attempts
        WHERE run_id = $1::uuid
          AND execution_id = ANY($2::uuid[])
        ORDER BY execution_id, attempt_no, id
        "#,
    )
    .bind(run_id)
    .bind(&execution_ids)
    .fetch_all(db)
    .await?;

    let aggregates = sqlx::query_as::<_, ExecutionAggregate>(
        r#"
        SELECT
            execution_id,
            run_id,
            attempt_id,
            overall_status::text as overall_status,
            aggregate_score,
            evaluator_result_count,
            dimension_scores,
            blocking_failures,
            summary,
            created_at,
            updated_at
        FROM execution_aggregates
        WHERE run_id = $1::uuid
          AND execution_id = ANY($2::uuid[])
        ORDER BY execution_id
        "#,
    )
    .bind(run_id)
    .bind(&execution_ids)
    .fetch_all(db)
    .await?;

    let attempt_ids = attempts
        .iter()
        .map(|attempt| attempt.id)
        .collect::<Vec<_>>();

    let evaluator_results = if attempt_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, EvaluatorResult>(
            r#"
            SELECT
                id,
                run_id,
                execution_id,
                attempt_id,
                evaluator_id,
                evaluator_version,
                evaluator_profile_id,
                evaluator_profile_version,
                evaluator_interface_version,
                evaluator_runtime_version,
                dimension,
                status::text as status,
                blocking,
                score_kind,
                raw_score,
                raw_score_min,
                raw_score_max,
                normalized_score,
                weight,
                severity::text as severity,
                failure_category,
                reason,
                evidence,
                raw_evaluator_output,
                created_at
            FROM evaluator_results
            WHERE run_id = $1::uuid
              AND attempt_id = ANY($2::uuid[])
            ORDER BY execution_id, attempt_id, created_at, id
            "#,
        )
        .bind(run_id)
        .bind(&attempt_ids)
        .fetch_all(db)
        .await?
    };

    Ok(RunExportBatch {
        executions,
        attempts,
        aggregates,
        evaluator_results,
    })
}

fn run_export_payload(
    run: &Run,
    summary: &RunResultsSummary,
    executions: &[Execution],
    attempts: &[ExecutionAttempt],
    aggregates: &[ExecutionAggregate],
    evaluator_results: &[EvaluatorResult],
) -> Value {
    let mut attempts_by_execution: BTreeMap<Uuid, Vec<&ExecutionAttempt>> = BTreeMap::new();
    for attempt in attempts {
        attempts_by_execution
            .entry(attempt.execution_id)
            .or_default()
            .push(attempt);
    }

    let mut aggregates_by_execution: BTreeMap<Uuid, &ExecutionAggregate> = BTreeMap::new();
    for aggregate in aggregates {
        aggregates_by_execution.insert(aggregate.execution_id, aggregate);
    }

    let mut results_by_attempt: BTreeMap<Uuid, Vec<&EvaluatorResult>> = BTreeMap::new();
    for result in evaluator_results {
        results_by_attempt
            .entry(result.attempt_id)
            .or_default()
            .push(result);
    }

    let exported_executions = executions
        .iter()
        .map(|execution| {
            let exported_attempts = attempts_by_execution
                .get(&execution.id)
                .into_iter()
                .flatten()
                .map(|attempt| {
                    let attempt_results = results_by_attempt
                        .get(&attempt.id)
                        .into_iter()
                        .flatten()
                        .map(|result| json!(result))
                        .collect::<Vec<_>>();

                    json!({
                        "attempt": attempt,
                        "evaluator_results": attempt_results,
                    })
                })
                .collect::<Vec<_>>();

            let aggregate = aggregates_by_execution
                .get(&execution.id)
                .map(|row| json!(row))
                .unwrap_or(Value::Null);

            json!({
                "execution": execution,
                "aggregate": aggregate,
                "attempts": exported_attempts,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "data": {
            "run": {
                "run_id": run.id,
                "run_key": run.run_key,
                "status": run.status,
                "gate_status": run.gate_status,
                "expected_execution_count": run.expected_execution_count,
                "terminal_execution_count": run.terminal_execution_count,
                "passed_execution_count": run.passed_execution_count,
                "failed_execution_count": run.failed_execution_count,
                "errored_execution_count": run.errored_execution_count,
                "summary": run.summary,
                "error_message": run.error_message,
                "created_at": run.created_at,
                "completed_at": run.completed_at,
                "updated_at": run.updated_at,
            },
            "results": {
                "execution_count": summary.execution_count,
                "aggregate_count": summary.aggregate_count,
                "missing_aggregate_count": summary.missing_aggregate_count,
                "status_counts": {
                    "passed": summary.passed_execution_count,
                    "failed": summary.failed_execution_count,
                    "error": summary.error_execution_count,
                    "skipped": summary.skipped_execution_count,
                },
                "score": {
                    "average": summary.average_score,
                    "min": summary.min_score,
                    "max": summary.max_score,
                },
                "evaluator_result_count": summary.evaluator_result_count,
                "blocking_failure_count": summary.blocking_failure_count,
            },
            "executions": exported_executions,
        },
        "meta": {
            "summary_only": false,
            "execution_count": executions.len(),
            "attempt_count": attempts.len(),
            "aggregate_count": aggregates.len(),
            "evaluator_result_count": evaluator_results.len(),
        }
    })
}

async fn handle_results(context: Context, run_id: String) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let db = context.db().await?;
    let out = context.out().await?;
    let run = select_existing_run(db, run_id).await?;
    let summary = select_run_results_summary(db, run_id).await?;
    let payload = run_results_payload(&run, &summary);

    out.write_value(&payload)?;
    Ok(())
}

async fn handle_status(context: Context, run_id: String) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let db = context.db().await?;
    let out = context.out().await?;
    let run = select_existing_run(db, run_id).await?;
    let payload = run_watch_payload(&run, is_terminal_run_status(&run.status));

    out.write_value(&payload)?;
    Ok(())
}

async fn handle_export(
    context: Context,
    run_id: String,
    format: RunExportFormat,
    batch_size: i64,
) -> anyhow::Result<()> {
    if batch_size <= 0 {
        anyhow::bail!("export batch_size must be greater than zero");
    }

    let run_id = parse_run_id(&run_id)?;
    let db = context.db().await?;
    let out = context.out().await?;

    let run = select_existing_run(db, run_id).await?;
    let summary = select_run_results_summary(db, run_id).await?;

    match format {
        RunExportFormat::Json => {
            let mut all_executions = Vec::new();
            let mut all_attempts = Vec::new();
            let mut all_aggregates = Vec::new();
            let mut all_evaluator_results = Vec::new();
            let mut cursor = None;

            loop {
                let execution_batch =
                    select_execution_batch_by_run_id(db, run_id, cursor, batch_size).await?;
                if execution_batch.is_empty() {
                    break;
                }

                cursor = execution_batch.last().map(|execution| execution.id);
                let batch =
                    select_run_export_batch_for_executions(db, run_id, execution_batch).await?;

                all_executions.extend(batch.executions);
                all_attempts.extend(batch.attempts);
                all_aggregates.extend(batch.aggregates);
                all_evaluator_results.extend(batch.evaluator_results);
            }

            let payload = run_export_payload(
                &run,
                &summary,
                &all_executions,
                &all_attempts,
                &all_aggregates,
                &all_evaluator_results,
            );
            out.write_value(&payload)?;
        }
        RunExportFormat::Jsonl => {
            let run_line = json!({
                "type": "run",
                "run": run,
            });
            out.write_line(serde_json::to_string(&run_line)?)?;

            let summary_line = json!({
                "type": "results_summary",
                "run_id": run_id,
                "results": {
                    "execution_count": summary.execution_count,
                    "aggregate_count": summary.aggregate_count,
                    "passed_execution_count": summary.passed_execution_count,
                    "failed_execution_count": summary.failed_execution_count,
                    "error_execution_count": summary.error_execution_count,
                    "skipped_execution_count": summary.skipped_execution_count,
                    "missing_aggregate_count": summary.missing_aggregate_count,
                    "evaluator_result_count": summary.evaluator_result_count,
                    "blocking_failure_count": summary.blocking_failure_count,
                    "average_score": summary.average_score,
                    "min_score": summary.min_score,
                    "max_score": summary.max_score,
                },
            });
            out.write_line(serde_json::to_string(&summary_line)?)?;

            let mut cursor = None;
            loop {
                let execution_batch =
                    select_execution_batch_by_run_id(db, run_id, cursor, batch_size).await?;
                if execution_batch.is_empty() {
                    break;
                }

                cursor = execution_batch.last().map(|execution| execution.id);
                let batch =
                    select_run_export_batch_for_executions(db, run_id, execution_batch).await?;

                let mut attempts_by_execution: BTreeMap<Uuid, Vec<&ExecutionAttempt>> =
                    BTreeMap::new();
                for attempt in &batch.attempts {
                    attempts_by_execution
                        .entry(attempt.execution_id)
                        .or_default()
                        .push(attempt);
                }

                let mut aggregates_by_execution: BTreeMap<Uuid, &ExecutionAggregate> =
                    BTreeMap::new();
                for aggregate in &batch.aggregates {
                    aggregates_by_execution.insert(aggregate.execution_id, aggregate);
                }

                let mut results_by_attempt: BTreeMap<Uuid, Vec<&EvaluatorResult>> = BTreeMap::new();
                for result in &batch.evaluator_results {
                    results_by_attempt
                        .entry(result.attempt_id)
                        .or_default()
                        .push(result);
                }

                for execution in &batch.executions {
                    let execution_line = json!({
                        "type": "execution",
                        "run_id": run_id,
                        "execution": execution,
                    });
                    out.write_line(serde_json::to_string(&execution_line)?)?;

                    if let Some(aggregate) = aggregates_by_execution.get(&execution.id) {
                        let aggregate_line = json!({
                            "type": "execution_aggregate",
                            "run_id": run_id,
                            "execution_id": execution.id,
                            "aggregate": aggregate,
                        });
                        out.write_line(serde_json::to_string(&aggregate_line)?)?;
                    }

                    for attempt in attempts_by_execution
                        .get(&execution.id)
                        .into_iter()
                        .flatten()
                    {
                        let attempt_line = json!({
                            "type": "execution_attempt",
                            "run_id": run_id,
                            "execution_id": execution.id,
                            "attempt": attempt,
                        });
                        out.write_line(serde_json::to_string(&attempt_line)?)?;

                        for result in results_by_attempt.get(&attempt.id).into_iter().flatten() {
                            let result_line = json!({
                                "type": "evaluator_result",
                                "run_id": run_id,
                                "execution_id": execution.id,
                                "attempt_id": attempt.id,
                                "evaluator_result": result,
                            });
                            out.write_line(serde_json::to_string(&result_line)?)?;
                        }
                    }
                }
            }

            out.flush()?;
        }
    }

    Ok(())
}

fn run_cancel_payload(outcome: &run_cancel::CancelRunOutcome) -> Value {
    json!({
        "data": {
            "run_id": outcome.run.id,
            "run_key": outcome.run.run_key,
            "status": outcome.run.status,
            "gate_status": outcome.run.gate_status,
            "expected_execution_count": outcome.run.expected_execution_count,
            "terminal_execution_count": outcome.run.terminal_execution_count,
            "passed_execution_count": outcome.run.passed_execution_count,
            "failed_execution_count": outcome.run.failed_execution_count,
            "errored_execution_count": outcome.run.errored_execution_count,
            "summary": outcome.run.summary,
            "error_message": outcome.run.error_message,
            "completed_at": outcome.run.completed_at,
            "updated_at": outcome.run.updated_at,
        },
        "meta": {
            "cancelled": outcome.cancelled,
            "already_cancelled": outcome.already_cancelled,
            "terminal": true,
            "chunks_cancelled": outcome.chunks_cancelled,
            "executions_cancelled": outcome.executions_cancelled,
            "attempts_cancelled": outcome.attempts_cancelled,
            "outbox_events_enqueued": outcome.outbox_events_enqueued,
        }
    })
}

async fn handle_cancel(context: Context, run_id: String) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let db = context.db().await?;
    let out = context.out().await?;
    let outcome = run_cancel::cancel_run(db, run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run '{}' was not found", run_id))?;

    out.write_value(&run_cancel_payload(&outcome))?;
    Ok(())
}

async fn select_existing_run_for_watch(db: &sqlx::PgPool, run_id: Uuid) -> anyhow::Result<Run> {
    runs::select_run_by_id(db, run_id).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "run '{}' was not found; watch only waits for runs that already exist",
            run_id
        )
    })
}

async fn handle_watch(
    context: Context,
    run_id: String,
    interval_seconds: u64,
    timeout_seconds: Option<u64>,
    fail_on_gate: bool,
) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let db = context.db().await?;
    let out = context.out().await?;
    let interval = Duration::from_secs(interval_seconds);
    let deadline = timeout_seconds.map(|seconds| Instant::now() + Duration::from_secs(seconds));
    let mut last_snapshot = None;
    let mut run = select_existing_run_for_watch(db, run_id).await?;

    loop {
        let terminal = is_terminal_run_status(&run.status);
        let snapshot = RunWatchSnapshotKey::from(&run);

        if last_snapshot.as_ref() != Some(&snapshot) || terminal {
            out.write_value(&run_watch_payload(&run, terminal))?;
            out.flush()?;
            last_snapshot = Some(snapshot);
        }

        if terminal {
            if let Some(reason) = run_terminal_failure_reason(&run) {
                anyhow::bail!(reason);
            }

            if fail_on_gate {
                if let Some(reason) = run_gate_failure_reason(&run) {
                    anyhow::bail!(reason);
                }
            }
            return Ok(());
        }

        let sleep_for = if let Some(deadline) = deadline {
            let now = Instant::now();
            if now >= deadline {
                anyhow::bail!(
                    "timed out watching run '{}' before terminal status; last status={}",
                    run_id,
                    run.status
                );
            }

            deadline.saturating_duration_since(now).min(interval)
        } else {
            interval
        };

        sleep(sleep_for).await;
        run = select_existing_run_for_watch(db, run_id).await?;
    }
}

#[derive(Debug, Subcommand)]
/// Run command operations.
///
/// `Create`, `Test`, `Watch`, `Status`, `Cancel`, `Results`, and `Export` are
/// implemented.
pub(crate) enum SubCommand {
    /// Create a run from profile + dataset inputs
    Create {
        /// Run profile YAML/JSON inline string
        #[arg(
            long,
            value_name = "YAML_OR_JSON",
            conflicts_with = "profile_file",
            required_unless_present = "profile_file"
        )]
        profile: Option<String>,

        /// Path to run profile YAML/JSON file
        #[arg(
            long,
            value_name = "FILE",
            value_parser = parse_filepath,
            conflicts_with = "profile",
            required_unless_present = "profile"
        )]
        profile_file: Option<PathBuf>,

        /// Dataset YAML/JSON inline string
        #[arg(
            long,
            value_name = "YAML_OR_JSON",
            conflicts_with = "dataset_file",
            required_unless_present = "dataset_file"
        )]
        dataset: Option<String>,

        /// Path to dataset YAML/JSON file
        #[arg(
            long,
            value_name = "FILE",
            value_parser = parse_filepath,
            conflicts_with = "dataset",
            required_unless_present = "dataset"
        )]
        dataset_file: Option<PathBuf>,
    },

    /// Parse and validate run profile + dataset inputs
    Test {
        /// Run profile YAML/JSON inline string
        #[arg(
            long,
            value_name = "YAML_OR_JSON",
            conflicts_with = "profile_file",
            required_unless_present = "profile_file"
        )]
        profile: Option<String>,

        /// Path to run profile YAML/JSON file
        #[arg(
            long,
            value_name = "FILE",
            value_parser = parse_filepath,
            conflicts_with = "profile",
            required_unless_present = "profile"
        )]
        profile_file: Option<PathBuf>,

        /// Dataset YAML/JSON inline string
        #[arg(
            long,
            value_name = "YAML_OR_JSON",
            conflicts_with = "dataset_file",
            required_unless_present = "dataset_file"
        )]
        dataset: Option<String>,

        /// Path to dataset YAML/JSON file
        #[arg(
            long,
            value_name = "FILE",
            value_parser = parse_filepath,
            conflicts_with = "dataset",
            required_unless_present = "dataset"
        )]
        dataset_file: Option<PathBuf>,
    },

    /// Watch run progress and stream status updates
    Watch {
        /// Run identifier to watch
        run_id: String,

        /// Polling interval in seconds
        #[arg(long, default_value_t = DEFAULT_WATCH_INTERVAL_SECONDS, value_parser = clap::value_parser!(u64).range(1..=MAX_WATCH_INTERVAL_SECONDS))]
        interval_seconds: u64,

        /// Maximum seconds to wait before failing
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=MAX_WATCH_TIMEOUT_SECONDS))]
        timeout_seconds: Option<u64>,

        /// Return success for terminal failed gates; useful for observation-only watches
        #[arg(long, default_value_t = false)]
        no_fail_on_gate: bool,
    },

    /// Show run status snapshot
    Status {
        /// Run identifier
        run_id: String,
    },

    /// Cancel an active run
    Cancel {
        /// Run identifier
        run_id: String,
    },

    /// Show run results summary
    Results {
        /// Run identifier
        run_id: String,
    },

    /// Export run results and artifacts
    Export {
        /// Run identifier
        run_id: String,

        /// Export serialization format; use `jsonl` for large runs.
        #[arg(long, value_enum, default_value_t = RunExportFormat::Json)]
        format: RunExportFormat,

        /// Number of executions fetched per export batch.
        #[arg(long, default_value_t = EXPORT_EXECUTION_BATCH_SIZE, value_parser = clap::value_parser!(i64).range(1..=MAX_EXPORT_BATCH_SIZE))]
        batch_size: i64,
    },
}

#[derive(Debug, Args)]
/// Arguments for `vigilo run`.
pub(crate) struct Command {
    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

#[async_trait]
impl Executable for Command {
    /// Dispatches run subcommands and reports reserved subcommands that are not
    /// implemented yet.
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self.command {
            Some(SubCommand::Create {
                profile,
                profile_file,
                dataset,
                dataset_file,
            }) => {
                info!("creating run from profile and dataset inputs");
                handle_create(context, profile, profile_file, dataset, dataset_file).await
            }
            Some(SubCommand::Status { run_id }) => {
                info!("fetching status for run {}", run_id);
                handle_status(context, run_id).await
            }
            Some(SubCommand::Cancel { run_id }) => {
                info!("cancelling run {}", run_id);
                handle_cancel(context, run_id).await
            }
            Some(SubCommand::Results { run_id }) => {
                info!("fetching results for run {}", run_id);
                handle_results(context, run_id).await
            }
            Some(SubCommand::Export {
                run_id,
                format,
                batch_size,
            }) => {
                info!("exporting run {}", run_id);
                handle_export(context, run_id, format, batch_size).await
            }
            Some(SubCommand::Watch {
                run_id,
                interval_seconds,
                timeout_seconds,
                no_fail_on_gate,
            }) => {
                info!("watching run {}", run_id);
                handle_watch(
                    context,
                    run_id,
                    interval_seconds,
                    timeout_seconds,
                    !no_fail_on_gate,
                )
                .await
            }
            Some(SubCommand::Test {
                profile,
                profile_file,
                dataset,
                dataset_file,
            }) => {
                info!("parsing run test profile and dataset inputs");
                handle_test(context, profile, profile_file, dataset, dataset_file).await
            }
            None => anyhow::bail!(
                "missing run subcommand; use `vigilo run test --profile-file <file> --dataset-file <file>`"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        EXPORT_EXECUTION_BATCH_SIZE,
        RunExportFormat,
        RunResultsSummary,
        build_chunks,
        canonical_json,
        handle_export,
        handle_status,
        handle_watch,
        is_terminal_run_status,
        parse_run_id,
        parse_structured_payload,
        read_inline_or_file,
        run_cancel_payload,
        run_export_payload,
        run_gate_failure_reason,
        run_results_payload,
        run_terminal_failure_reason,
        run_watch_payload,
    };
    use crate::{
        context::{
            Context,
            output::OutputFormat,
            wasm,
        },
        models::{
            evaluator_result::EvaluatorResult,
            execution::Execution,
            execution_aggregate::ExecutionAggregate,
            execution_attempt::ExecutionAttempt,
            run::Run,
        },
    };

    #[test]
    fn read_inline_or_file_prefers_inline() {
        let raw = read_inline_or_file(Some("k: v".to_string()), None, "profile").unwrap();
        assert_eq!(raw, "k: v");
    }

    #[test]
    fn parse_structured_payload_accepts_yaml_and_json() {
        let yaml = parse_structured_payload("a: 1", "profile").unwrap();
        assert_eq!(yaml.get("a").and_then(|v| v.as_i64()), Some(1));

        let json = parse_structured_payload("{\"a\":1}", "dataset").unwrap();
        assert_eq!(json.get("a").and_then(|v| v.as_i64()), Some(1));
    }

    #[test]
    fn canonical_json_sorts_object_keys_recursively() {
        let value = json!({"b": 1, "a": {"d": 1, "c": 2}});
        let canonical = canonical_json(&value);
        let encoded = serde_json::to_string(&canonical).unwrap();
        assert_eq!(encoded, "{\"a\":{\"c\":2,\"d\":1},\"b\":1}");
    }

    #[test]
    fn build_chunks_creates_expected_boundaries() {
        let chunks = build_chunks(205, 100);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].ordinal_start, 0);
        assert_eq!(chunks[0].ordinal_end, 100);
        assert_eq!(chunks[2].ordinal_start, 200);
        assert_eq!(chunks[2].ordinal_end, 205);
    }

    fn run_with_status(status: &str, gate_status: &str) -> Run {
        let now = Utc::now();
        Run {
            id: Uuid::now_v7(),
            run_key: "run-key".to_string(),
            name: None,
            description: None,
            dataset_id: Uuid::now_v7(),
            dataset_version: "v1".to_string(),
            evaluation_profile_id: "profile".to_string(),
            evaluation_profile_version: "v1".to_string(),
            aggregation_policy_id: "policy".to_string(),
            aggregation_policy_version: "v1".to_string(),
            agent_provider: "provider".to_string(),
            agent_name: "agent".to_string(),
            agent_version: None,
            prompt_config_id: "prompt".to_string(),
            prompt_config_version: "v1".to_string(),
            config_snapshot: json!({}),
            status: status.to_string(),
            gate_status: gate_status.to_string(),
            coordinator_id: None,
            coordinator_leased_until: None,
            coordinator_heartbeat_at: None,
            expected_execution_count: 1,
            terminal_execution_count: 1,
            passed_execution_count: if gate_status == "pass" { 1 } else { 0 },
            failed_execution_count: if gate_status == "fail" { 1 } else { 0 },
            errored_execution_count: 0,
            summary: json!({}),
            error_message: None,
            created_at: now,
            started_at: Some(now),
            dispatched_at: Some(now),
            finalized_at: Some(now),
            completed_at: Some(now),
            updated_at: now,
        }
    }

    #[test]
    fn terminal_run_statuses_are_gating_boundaries() {
        assert!(is_terminal_run_status("completed"));
        assert!(is_terminal_run_status("failed"));
        assert!(is_terminal_run_status("cancelled"));
        assert!(!is_terminal_run_status("pending"));
        assert!(!is_terminal_run_status("running"));
        assert!(!is_terminal_run_status("finalizing"));
    }

    #[test]
    fn gate_failure_reason_only_allows_completed_pass() {
        assert!(run_gate_failure_reason(&run_with_status("completed", "pass")).is_none());
        assert!(run_gate_failure_reason(&run_with_status("completed", "fail")).is_some());
        assert!(run_gate_failure_reason(&run_with_status("completed", "unknown")).is_some());
        assert!(run_gate_failure_reason(&run_with_status("failed", "unknown")).is_none());
        assert!(run_gate_failure_reason(&run_with_status("cancelled", "unknown")).is_none());
        assert!(run_gate_failure_reason(&run_with_status("running", "unknown")).is_none());
    }

    #[test]
    fn terminal_failure_reason_flags_failed_and_cancelled_runs() {
        assert!(run_terminal_failure_reason(&run_with_status("completed", "pass")).is_none());
        assert!(run_terminal_failure_reason(&run_with_status("completed", "fail")).is_none());
        assert!(run_terminal_failure_reason(&run_with_status("failed", "unknown")).is_some());
        assert!(run_terminal_failure_reason(&run_with_status("cancelled", "unknown")).is_some());
    }

    #[test]
    fn parse_run_id_rejects_non_uuid_values() {
        assert!(parse_run_id(&Uuid::now_v7().to_string()).is_ok());
        assert!(parse_run_id("not-a-run-id").is_err());
    }

    #[test]
    fn run_results_payload_is_summary_only() {
        let run = run_with_status("completed", "fail");
        let summary = RunResultsSummary {
            execution_count: 10,
            aggregate_count: 8,
            passed_execution_count: 5,
            failed_execution_count: 3,
            error_execution_count: 0,
            skipped_execution_count: 0,
            missing_aggregate_count: 2,
            evaluator_result_count: 24,
            blocking_failure_count: 3,
            average_score: Some(0.72),
            min_score: Some(0.2),
            max_score: Some(1.0),
        };
        let payload = run_results_payload(&run, &summary);

        assert_eq!(payload["meta"]["summary_only"], json!(true));
        assert!(payload["data"]["executions"].is_null());
        assert_eq!(payload["data"]["results"]["execution_count"], json!(10));
        assert_eq!(
            payload["data"]["results"]["missing_aggregate_count"],
            json!(2)
        );
        assert_eq!(
            payload["data"]["results"]["status_counts"]["failed"],
            json!(3)
        );
    }

    #[test]
    fn run_cancel_payload_reports_terminal_cancel_state() {
        let run = run_with_status("cancelled", "fail");
        let outcome = crate::db::workflows::run_cancel::CancelRunOutcome {
            run,
            cancelled: true,
            already_cancelled: false,
            chunks_cancelled: 3,
            executions_cancelled: 2,
            attempts_cancelled: 1,
            outbox_events_enqueued: 1,
        };
        let payload = run_cancel_payload(&outcome);

        assert_eq!(payload["data"]["status"], json!("cancelled"));
        assert_eq!(payload["data"]["gate_status"], json!("fail"));
        assert_eq!(payload["meta"]["terminal"], json!(true));
        assert_eq!(payload["meta"]["cancelled"], json!(true));
        assert_eq!(payload["meta"]["chunks_cancelled"], json!(3));
        assert_eq!(payload["meta"]["outbox_events_enqueued"], json!(1));
    }

    #[tokio::test]
    async fn watch_rejects_malformed_run_id_before_database_initialization() {
        let context = Context::new(
            "not-a-postgres-url".to_string(),
            1,
            "not-used".to_string(),
            wasm::Config::default(),
            OutputFormat::Json,
        );

        let err = handle_watch(context, "not-a-run-id".to_string(), 1, Some(1), true)
            .await
            .unwrap_err();

        assert!(err.to_string().starts_with("invalid run_id"));
    }

    #[test]
    fn run_watch_payload_marks_completed_pass_as_gate_passed() {
        let run = run_with_status("completed", "pass");
        let payload = run_watch_payload(&run, true);

        assert_eq!(payload["data"]["run_id"], json!(run.id));
        assert_eq!(payload["meta"]["terminal"], json!(true));
        assert_eq!(payload["meta"]["gate_passed"], json!(true));
    }

    #[test]
    fn run_export_payload_includes_nested_execution_data() {
        let now = Utc::now();
        let run = run_with_status("completed", "pass");
        let execution_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        let evaluator_result_id = Uuid::now_v7();

        let summary = RunResultsSummary {
            execution_count: 1,
            aggregate_count: 1,
            passed_execution_count: 1,
            failed_execution_count: 0,
            error_execution_count: 0,
            skipped_execution_count: 0,
            missing_aggregate_count: 0,
            evaluator_result_count: 1,
            blocking_failure_count: 0,
            average_score: Some(1.0),
            min_score: Some(1.0),
            max_score: Some(1.0),
        };

        let executions = vec![Execution {
            id: execution_id,
            run_id: run.id,
            case_id: Uuid::now_v7(),
            task_type: "chat".to_string(),
            tags: json!(["smoke"]),
            input_payload: json!({"prompt": "hello"}),
            expected_output: json!({"intent": "greet"}),
            case_metadata: json!({"suite": "base"}),
            evaluation_profile_id: "profile".to_string(),
            evaluation_profile_version: "v1".to_string(),
            evaluator_manifest: json!([]),
            expected_evaluator_count: 1,
            status: "completed".to_string(),
            current_attempt_no: 1,
            current_attempt_id: Some(attempt_id),
            last_error_message: None,
            created_at: now,
            started_at: Some(now),
            completed_at: Some(now),
            updated_at: now,
        }];

        let attempts = vec![ExecutionAttempt {
            id: attempt_id,
            execution_id,
            run_id: run.id,
            attempt_no: 1,
            status: "completed".to_string(),
            worker_id: None,
            worker_host: None,
            queue_message_id: None,
            leased_until: None,
            heartbeat_at: None,
            request_artifact_uri: Some("s3://bucket/request.json".to_string()),
            response_artifact_uri: Some("s3://bucket/response.json".to_string()),
            agent_latency_ms: Some(12),
            evaluator_latency_ms: Some(4),
            total_latency_ms: Some(16),
            token_usage: json!({"input": 10, "output": 4}),
            outcome_summary: json!({"status": "ok"}),
            error_message: None,
            created_at: now,
            started_at: Some(now),
            completed_at: Some(now),
            updated_at: now,
        }];

        let aggregates = vec![ExecutionAggregate {
            execution_id,
            run_id: run.id,
            attempt_id,
            overall_status: "passed".to_string(),
            aggregate_score: Some(1.0),
            evaluator_result_count: 1,
            dimension_scores: json!({"quality": 1.0}),
            blocking_failures: json!([]),
            summary: json!({"overall_status": "passed"}),
            created_at: now,
            updated_at: now,
        }];

        let evaluator_results = vec![EvaluatorResult {
            id: evaluator_result_id,
            run_id: run.id,
            execution_id,
            attempt_id,
            evaluator_id: Uuid::now_v7(),
            evaluator_version: "1.0.0".to_string(),
            evaluator_profile_id: "profile".to_string(),
            evaluator_profile_version: "v1".to_string(),
            evaluator_interface_version: Some("1.0".to_string()),
            evaluator_runtime_version: Some("1.0".to_string()),
            dimension: "quality".to_string(),
            status: "passed".to_string(),
            blocking: true,
            score_kind: "continuous".to_string(),
            raw_score: Some(1.0),
            raw_score_min: Some(0.0),
            raw_score_max: Some(1.0),
            normalized_score: Some(1.0),
            weight: 1.0,
            severity: "none".to_string(),
            failure_category: None,
            reason: Some("all checks passed".to_string()),
            evidence: json!({"span": "ok"}),
            raw_evaluator_output: json!({"result": "pass"}),
            created_at: now,
        }];

        let payload = run_export_payload(
            &run,
            &summary,
            &executions,
            &attempts,
            &aggregates,
            &evaluator_results,
        );

        assert_eq!(payload["meta"]["summary_only"], json!(false));
        assert_eq!(payload["meta"]["execution_count"], json!(1));
        assert_eq!(payload["meta"]["attempt_count"], json!(1));
        assert_eq!(payload["meta"]["evaluator_result_count"], json!(1));
        assert_eq!(
            payload["data"]["executions"][0]["execution"]["id"],
            json!(execution_id)
        );
        assert_eq!(
            payload["data"]["executions"][0]["attempts"][0]["attempt"]["id"],
            json!(attempt_id)
        );
        assert_eq!(
            payload["data"]["executions"][0]["attempts"][0]["evaluator_results"][0]["id"],
            json!(evaluator_result_id)
        );
        assert_eq!(
            payload["data"]["executions"][0]["aggregate"]["execution_id"],
            json!(execution_id)
        );
    }

    #[tokio::test]
    async fn export_rejects_malformed_run_id_before_database_initialization() {
        let context = Context::new(
            "not-a-postgres-url".to_string(),
            1,
            "not-used".to_string(),
            wasm::Config::default(),
            OutputFormat::Json,
        );

        let err = handle_export(
            context,
            "not-a-run-id".to_string(),
            RunExportFormat::Json,
            EXPORT_EXECUTION_BATCH_SIZE,
        )
        .await
        .unwrap_err();

        assert!(err.to_string().starts_with("invalid run_id"));
    }

    #[tokio::test]
    async fn status_rejects_malformed_run_id_before_database_initialization() {
        let context = Context::new(
            "not-a-postgres-url".to_string(),
            1,
            "not-used".to_string(),
            wasm::Config::default(),
            OutputFormat::Json,
        );

        let err = handle_status(context, "not-a-run-id".to_string())
            .await
            .unwrap_err();

        assert!(err.to_string().starts_with("invalid run_id"));
    }
}
