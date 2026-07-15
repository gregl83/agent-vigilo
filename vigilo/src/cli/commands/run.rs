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
            run_export as run_export_workflow,
            run_profile_validation,
            run_results::{
                self as run_results_workflow,
                RunResultsSummary,
            },
            run_status as run_status_workflow,
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
        run_chunk::{
            RunChunkDraft,
            run_shard_for_chunk_index,
        },
    },
};

mod cancel;
mod create;
mod export;
mod results;
mod status;
mod test;
mod watch;

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
            "case_group": case.case_group.clone(),
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
            case_group: case.case_group.clone(),
            input_payload: blob_payload["input"].clone(),
            expected_output: blob_payload["expected_output"].clone(),
            context_payload: blob_payload["context"].clone(),
            tags: blob_payload["tags"].clone(),
            metadata: blob_payload["metadata"].clone(),
        });

        dataset_cases.push(DatasetVersionCaseDraft {
            case_id: case.id,
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
///
/// Chunks are ordinal scheduling ranges and may contain mixed case groups.
/// Per-case profile routing is resolved later by workers from stored case data.
fn build_chunks(total_cases: usize, chunk_size: usize) -> Vec<RunChunkDraft> {
    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < total_cases {
        let end = (start + chunk_size).min(total_cases);
        let chunk_index = chunks.len();
        chunks.push(RunChunkDraft {
            chunk_id: Uuid::now_v7(),
            run_shard: run_shard_for_chunk_index(chunk_index),
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunWatchSnapshotKey {
    status: String,
    gate_status: String,
    expected_execution_count: i32,
    terminal_execution_count: i32,
    passed_execution_count: i32,
    failed_execution_count: i32,
    errored_execution_count: i32,
    live_expected_execution_count: i64,
    live_execution_count: i64,
    live_terminal_execution_count: i64,
    live_passed_execution_count: i64,
    live_failed_execution_count: i64,
    live_errored_execution_count: i64,
    live_cancelled_chunk_count: i64,
    shard_summary_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum RunExportFormat {
    Json,
    Jsonl,
}

impl From<&Run> for RunWatchSnapshotKey {
    fn from(run: &Run) -> Self {
        Self::from(&run_status_workflow::RunStatusProjection::from_control_run(
            run.clone(),
        ))
    }
}

impl From<&run_status_workflow::RunStatusProjection> for RunWatchSnapshotKey {
    fn from(status: &run_status_workflow::RunStatusProjection) -> Self {
        Self {
            status: status.run.status.clone(),
            gate_status: status.run.gate_status.clone(),
            expected_execution_count: status.run.expected_execution_count,
            terminal_execution_count: status.run.terminal_execution_count,
            passed_execution_count: status.run.passed_execution_count,
            failed_execution_count: status.run.failed_execution_count,
            errored_execution_count: status.run.errored_execution_count,
            live_expected_execution_count: status.live_progress.expected_execution_count,
            live_execution_count: status.live_progress.execution_count,
            live_terminal_execution_count: status.live_progress.terminal_execution_count,
            live_passed_execution_count: status.live_progress.passed_execution_count,
            live_failed_execution_count: status.live_progress.failed_execution_count,
            live_errored_execution_count: status.live_progress.errored_execution_count,
            live_cancelled_chunk_count: status.live_progress.cancelled_chunk_count,
            shard_summary_count: status.shard_summary_count,
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

#[cfg(test)]
fn run_watch_payload(run: &Run, terminal: bool) -> Value {
    let status = run_status_workflow::RunStatusProjection::from_control_run(run.clone());
    run_watch_payload_from_status(&status, terminal)
}

fn run_watch_payload_from_status(
    status: &run_status_workflow::RunStatusProjection,
    terminal: bool,
) -> Value {
    let run = &status.run;
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
            "live_progress": {
                "expected_execution_count": status.live_progress.expected_execution_count,
                "execution_count": status.live_progress.execution_count,
                "terminal_execution_count": status.live_progress.terminal_execution_count,
                "passed_execution_count": status.live_progress.passed_execution_count,
                "failed_execution_count": status.live_progress.failed_execution_count,
                "errored_execution_count": status.live_progress.errored_execution_count,
                "skipped_execution_count": status.live_progress.skipped_execution_count,
                "missing_aggregate_count": status.live_progress.missing_aggregate_count,
                "failed_chunk_count": status.live_progress.failed_chunk_count,
                "cancelled_chunk_count": status.live_progress.cancelled_chunk_count,
            },
        },
        "meta": {
            "terminal": terminal,
            "gate_passed": run.status == "completed" && run.gate_status == "pass",
            "progress_source": status.progress_source(),
            "live_progress_complete": status.live_progress_complete(),
            "execution_route_count": status.execution_route_count,
            "shard_summary_count": status.shard_summary_count,
        }
    })
}

async fn select_existing_run(db: &sqlx::PgPool, run_id: Uuid) -> anyhow::Result<Run> {
    runs::select_run_by_id(db, run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run '{}' was not found", run_id))
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
                create::exec(context, profile, profile_file, dataset, dataset_file).await
            }
            Some(SubCommand::Status { run_id }) => {
                info!("fetching status for run {}", run_id);
                status::exec(context, run_id).await
            }
            Some(SubCommand::Cancel { run_id }) => {
                info!("cancelling run {}", run_id);
                cancel::exec(context, run_id).await
            }
            Some(SubCommand::Results { run_id }) => {
                info!("fetching results for run {}", run_id);
                results::exec(context, run_id).await
            }
            Some(SubCommand::Export {
                run_id,
                format,
                batch_size,
            }) => {
                info!("exporting run {}", run_id);
                export::exec(context, run_id, format, batch_size).await
            }
            Some(SubCommand::Watch {
                run_id,
                interval_seconds,
                timeout_seconds,
                no_fail_on_gate,
            }) => {
                info!("watching run {}", run_id);
                watch::exec(
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
                test::exec(context, profile, profile_file, dataset, dataset_file).await
            }
            None => anyhow::bail!(
                "missing run subcommand; use `vigilo run test --profile-file <file> --dataset-file <file>`"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        EXPORT_EXECUTION_BATCH_SIZE,
        RunExportFormat,
        RunResultsSummary,
        build_case_plans,
        build_chunks,
        cancel,
        canonical_json,
        export,
        is_terminal_run_status,
        parse_run_id,
        parse_structured_payload,
        read_inline_or_file,
        results,
        run_gate_failure_reason,
        run_status_workflow,
        run_terminal_failure_reason,
        run_watch_payload,
        run_watch_payload_from_status,
        status,
        watch,
    };
    use crate::{
        context::{
            Context,
            database::PlacementConfig,
            output::OutputFormat,
            wasm,
        },
        contracts::run::{
            DatasetCase,
            RunDataset,
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
        assert_eq!(chunks[0].run_shard, 0);
        assert_eq!(chunks[1].run_shard, 1);
        assert_eq!(chunks[2].run_shard, 2);
        assert_eq!(chunks[2].ordinal_start, 200);
        assert_eq!(chunks[2].ordinal_end, 205);
    }

    #[test]
    fn build_chunks_assigns_128_logical_shards_round_robin() {
        let chunks = build_chunks(129, 1);
        assert_eq!(chunks[0].run_shard, 0);
        assert_eq!(chunks[127].run_shard, 127);
        assert_eq!(chunks[128].run_shard, 0);
    }

    fn run_dataset_with_case_group(case_group: Option<&str>) -> RunDataset {
        RunDataset {
            dataset_id: Uuid::parse_str("018f1111-1111-7111-8111-111111111111").unwrap(),
            dataset_version: Some("1.0.0".to_string()),
            cases: vec![DatasetCase {
                id: Uuid::parse_str("018f1111-1111-7111-8111-111111111101").unwrap(),
                task_type: "classification".to_string(),
                case_group: case_group.map(ToOwned::to_owned),
                input: json!({"user_message": "I love this product."}),
                expected: Some(json!({"label": "positive"})),
                context: None,
                tags: vec!["sentiment".to_string()],
                metadata: BTreeMap::new(),
            }],
        }
    }

    #[test]
    fn build_case_plans_persists_case_group_override() {
        let dataset = run_dataset_with_case_group(Some("sentiment_classification"));

        let (case_blobs, _) = build_case_plans(&dataset).unwrap();

        assert_eq!(
            case_blobs[0].case_group.as_deref(),
            Some("sentiment_classification")
        );
    }

    #[test]
    fn build_case_plans_hash_changes_when_only_case_group_changes() {
        let with_group = run_dataset_with_case_group(Some("sentiment_classification"));
        let without_group = run_dataset_with_case_group(None);

        let (with_group_blobs, _) = build_case_plans(&with_group).unwrap();
        let (without_group_blobs, _) = build_case_plans(&without_group).unwrap();

        assert_ne!(
            with_group_blobs[0].case_hash,
            without_group_blobs[0].case_hash
        );
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
        let payload = results::run_results_payload(&run, &summary);

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
        let payload = cancel::run_cancel_payload(&outcome);

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
            PlacementConfig::default_single_database(),
            "not-used".to_string(),
            wasm::Config::default(),
            OutputFormat::Json,
        );

        let err = watch::exec(context, "not-a-run-id".to_string(), 1, Some(1), true)
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
    fn run_watch_payload_includes_live_routed_progress() {
        let run = run_with_status("running", "unknown");
        let status = run_status_workflow::RunStatusProjection {
            run,
            live_progress: run_status_workflow::RunProgressSummary {
                expected_execution_count: 10,
                execution_count: 7,
                terminal_execution_count: 5,
                passed_execution_count: 4,
                failed_execution_count: 1,
                errored_execution_count: 0,
                skipped_execution_count: 0,
                missing_aggregate_count: 0,
                failed_chunk_count: 0,
                cancelled_chunk_count: 1,
            },
            execution_route_count: 2,
            shard_summary_count: 1,
        };

        let payload = run_watch_payload_from_status(&status, false);

        assert_eq!(
            payload["data"]["live_progress"]["terminal_execution_count"],
            json!(5)
        );
        assert_eq!(
            payload["data"]["live_progress"]["cancelled_chunk_count"],
            json!(1)
        );
        assert_eq!(
            payload["meta"]["progress_source"],
            json!("execution_shards")
        );
        assert_eq!(payload["meta"]["live_progress_complete"], json!(false));
        assert_eq!(payload["meta"]["execution_route_count"], json!(2));
        assert_eq!(payload["meta"]["shard_summary_count"], json!(1));
    }

    #[test]
    fn run_export_payload_includes_nested_execution_data() {
        let now = Utc::now();
        let run = run_with_status("completed", "pass");
        let execution_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        let evaluator_result_id = Uuid::now_v7();
        let chunk_id = Uuid::now_v7();
        let run_shard = 0;

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
            run_shard,
            chunk_id,
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
            retry_after: None,
            retry_count: 0,
            last_attempt_completed_at: Some(now),
            created_at: now,
            started_at: Some(now),
            completed_at: Some(now),
            updated_at: now,
        }];

        let attempts = vec![ExecutionAttempt {
            id: attempt_id,
            execution_id,
            run_id: run.id,
            run_shard,
            attempt_no: 1,
            status: "completed".to_string(),
            worker_id: None,
            worker_host: None,
            queue_message_id: None,
            broker_message_id: None,
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
            run_shard,
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
            run_shard,
            execution_id,
            attempt_id,
            evaluator_id: Uuid::now_v7(),
            finding_index: 0,
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

        let payload = export::run_export_payload(
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
            PlacementConfig::default_single_database(),
            "not-used".to_string(),
            wasm::Config::default(),
            OutputFormat::Json,
        );

        let err = export::exec(
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
            PlacementConfig::default_single_database(),
            "not-used".to_string(),
            wasm::Config::default(),
            OutputFormat::Json,
        );

        let err = status::exec(context, "not-a-run-id".to_string())
            .await
            .unwrap_err();

        assert!(err.to_string().starts_with("invalid run_id"));
    }
}
