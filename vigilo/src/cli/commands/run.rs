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
            run_create,
            run_profile_validation,
        },
    },
    models::{
        case_blob::CaseBlobDraft,
        dataset_version_case::DatasetVersionCaseDraft,
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
) -> anyhow::Result<String> {
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

    hash_json(&json!({
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

    let run_dataset_id = parsed
        .dataset
        .dataset_id
        .clone()
        .and_then(|raw| Uuid::parse_str(&raw).ok())
        .unwrap_or_else(Uuid::now_v7)
        .to_string();

    let snapshot = json!({
        "profile": profile_payload,
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
        dataset_id: run_dataset_id,
        dataset_version: parsed
            .dataset
            .dataset_version
            .clone()
            .unwrap_or_else(|| dataset_version_id.clone()),
        dataset_version_id: dataset_version_id.clone(),
        evaluation_profile_id: parsed.profile.profile_id.clone(),
        evaluation_profile_version: parsed.profile.profile_version.clone(),
        profile_version_id: profile_version_id.clone(),
        profile_hash: profile_hash.clone(),
        aggregation_policy_id: "profile_case_group_aggregation".to_string(),
        aggregation_policy_version: "v3".to_string(),
        aggregation_policy_hash: aggregation_policy_hash.clone(),
        agent_provider: "unknown".to_string(),
        agent_name: "unknown".to_string(),
        agent_version: None,
        prompt_config_id: "default".to_string(),
        prompt_config_version: "v1".to_string(),
        config_snapshot: snapshot,
        expected_execution_count: dataset_cases.len() as i32,
    };

    let mut tx = db.begin().await?;

    run_create::bulk_insert_case_blobs(&mut tx, &case_blobs).await?;
    run_create::upsert_dataset_version(
        &mut tx,
        &dataset_version_id,
        &run_draft.dataset_id,
        &run_draft.dataset_version,
    )
    .await?;
    run_create::bulk_insert_dataset_membership(&mut tx, &dataset_version_id, &dataset_cases)
        .await?;
    run_create::insert_run_create(&mut tx, run_id, &run_draft).await?;
    run_create::bulk_insert_run_chunks(&mut tx, run_id, &dataset_version_id, &chunks).await?;

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

    out.write_line(serde_json::to_string_pretty(&payload)?)?;
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

    out.write_line(serde_json::to_string_pretty(&payload)?)?;
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

async fn handle_results(context: Context, run_id: String) -> anyhow::Result<()> {
    let run_id = parse_run_id(&run_id)?;
    let db = context.db().await?;
    let out = context.out().await?;
    let run = select_existing_run(db, run_id).await?;
    let summary = select_run_results_summary(db, run_id).await?;
    let payload = run_results_payload(&run, &summary);

    out.write_line(serde_json::to_string_pretty(&payload)?)?;
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
            out.write_line(serde_json::to_string_pretty(&run_watch_payload(
                &run, terminal,
            ))?)?;
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
/// `Create`, `Test`, `Watch`, and `Results` are implemented. Operational
/// commands (`Status`, `Cancel`, `Export`) are reserved and currently return
/// explicit not-implemented errors.
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
                anyhow::bail!("run status is not implemented yet")
            }
            Some(SubCommand::Cancel { run_id }) => {
                info!("cancelling run {}", run_id);
                anyhow::bail!("run cancel is not implemented yet")
            }
            Some(SubCommand::Results { run_id }) => {
                info!("fetching results for run {}", run_id);
                handle_results(context, run_id).await
            }
            Some(SubCommand::Export { run_id }) => {
                info!("exporting run {}", run_id);
                anyhow::bail!("run export is not implemented yet")
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
        RunResultsSummary,
        build_chunks,
        canonical_json,
        handle_watch,
        is_terminal_run_status,
        parse_run_id,
        parse_structured_payload,
        read_inline_or_file,
        run_gate_failure_reason,
        run_results_payload,
        run_terminal_failure_reason,
        run_watch_payload,
    };
    use crate::{
        context::{
            Context,
            wasm,
        },
        models::run::Run,
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

    #[tokio::test]
    async fn watch_rejects_malformed_run_id_before_database_initialization() {
        let context = Context::new(
            "not-a-postgres-url".to_string(),
            1,
            "not-used".to_string(),
            wasm::Config::default(),
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
}
