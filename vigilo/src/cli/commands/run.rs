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
    db::run_planning,
    models::{
        case_blob::CaseBlobDraft,
        dataset_version_case::DatasetVersionCaseDraft,
        run::RunDraft,
        run_chunk::RunChunkDraft,
    },
};

const DEFAULT_CHUNK_SIZE: usize = 100;

struct ParsedRunInputs {
    profile_payload: Value,
    dataset_payload: Value,
    profile: RunProfile,
    dataset: RunDataset,
}

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

fn parse_structured_payload(raw: &str, field: &str) -> anyhow::Result<Value> {
    serde_yaml::from_str::<Value>(raw)
        .map_err(|err| anyhow::anyhow!("invalid {} payload (yaml/json expected): {}", field, err))
}

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

fn hash_json(value: &Value) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(value)?;
    let mut hasher = Hasher::new();
    hasher.update(&bytes);
    Ok(hasher.finalize().to_hex().to_string())
}

fn canonical_tags(tags: &[String]) -> Value {
    let mut ordered = tags.to_vec();
    ordered.sort();
    Value::Array(ordered.into_iter().map(Value::String).collect())
}

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

    let (case_blobs, dataset_cases) = build_case_plans(&parsed.dataset)?;
    let dataset_version_id = compute_dataset_version_id(&parsed.dataset, &dataset_cases)?;
    let profile_hash = hash_json(&profile_payload)?;
    let aggregation_policy_hash = compute_aggregation_policy_hash(&parsed.profile)?;
    let profile_version_id = format!(
        "{}/{}",
        parsed.profile.profile_id, parsed.profile.profile_version
    );

    let chunk_size = DEFAULT_CHUNK_SIZE;
    let chunks = build_chunks(dataset_cases.len(), chunk_size);
    let run_id = Uuid::now_v7();
    let run_key = run_id.to_string();

    let legacy_dataset_id = parsed
        .dataset
        .dataset_id
        .clone()
        .and_then(|raw| Uuid::parse_str(&raw).ok())
        .unwrap_or_else(Uuid::now_v7)
        .to_string();

    let snapshot = json!({
        "profile": profile_payload,
        "dataset": dataset_payload,
        "dataset_version_id": dataset_version_id,
        "profile_version_id": profile_version_id,
        "profile_hash": profile_hash,
        "aggregation_policy_hash": aggregation_policy_hash,
        "chunk_size": chunk_size,
    });

    let run_draft = RunDraft {
        run_key: run_key.clone(),
        name: None,
        description: None,
        dataset_id: legacy_dataset_id,
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

    run_planning::bulk_insert_case_blobs(&mut tx, &case_blobs).await?;
    run_planning::upsert_dataset_version(
        &mut tx,
        &dataset_version_id,
        &run_draft.dataset_id,
        &run_draft.dataset_version,
    )
    .await?;
    run_planning::bulk_insert_dataset_membership(&mut tx, &dataset_version_id, &dataset_cases)
        .await?;
    run_planning::insert_run_create(&mut tx, run_id, &run_draft).await?;
    run_planning::bulk_insert_run_chunks(&mut tx, run_id, &dataset_version_id, &chunks).await?;
    run_planning::bulk_enqueue_chunk_events(&mut tx, run_id, &chunks).await?;

    tx.commit().await?;

    let payload = json!({
        "data": {
            "run_id": run_id,
            "run_key": run_key,
            "dataset_version_id": dataset_version_id,
            "profile_version_id": profile_version_id,
            "profile_hash": profile_hash,
            "aggregation_policy_hash": aggregation_policy_hash,
            "status": "pending",
        },
        "meta": {
            "case_count": dataset_cases.len(),
            "chunk_count": chunks.len(),
            "chunk_size": chunk_size,
        }
    });

    out.write_line(serde_json::to_string_pretty(&payload)?)?;
    Ok(())
}

async fn handle_test(
    context: Context,
    profile: Option<String>,
    profile_file: Option<PathBuf>,
    dataset: Option<String>,
    dataset_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    let out = context.out().await?;
    let parsed = load_run_inputs(profile, profile_file, dataset, dataset_file)?;

    let payload = json!({
        "data": {
            "profile": parsed.profile,
            "dataset": parsed.dataset,
        },
        "meta": {
            "profile_case_groups": parsed.profile.case_groups.len(),
            "dataset_cases": parsed.dataset.cases.len(),
            "sources": {
                "profile": if parsed.profile_payload.is_object() || parsed.profile_payload.is_array() { "structured" } else { "scalar" },
                "dataset": if parsed.dataset_payload.is_object() || parsed.dataset_payload.is_array() { "structured" } else { "scalar" },
            }
        }
    });

    out.write_line(serde_json::to_string_pretty(&payload)?)?;
    Ok(())
}

#[derive(Debug, Subcommand)]
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
pub(crate) struct Command {
    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

#[async_trait]
impl Executable for Command {
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

                // TODO: Query run + execution aggregate state from persistence.
                // TODO: Return machine-readable status payload.
                anyhow::bail!("run status is not implemented yet")
            }
            Some(SubCommand::Cancel { run_id }) => {
                info!("cancelling run {}", run_id);

                // TODO: Transition run to cancelled if still non-terminal.
                // TODO: Mark in-flight executions/attempts for cooperative stop.
                anyhow::bail!("run cancel is not implemented yet")
            }
            Some(SubCommand::Results { run_id }) => {
                info!("fetching results for run {}", run_id);

                // TODO: Read execution aggregates and evaluator summaries.
                // TODO: Emit machine-readable results payload.
                anyhow::bail!("run results is not implemented yet")
            }
            Some(SubCommand::Export { run_id }) => {
                info!("exporting run {}", run_id);

                // TODO: Support export format selection and output destinations.
                // TODO: Stream run results/evidence into export artifact.
                anyhow::bail!("run export is not implemented yet")
            }
            Some(SubCommand::Watch { run_id }) => {
                info!("watching run {}", run_id);

                // TODO: Poll run/execution state changes with backoff until terminal state.
                // TODO: Stream incremental progress snapshots to output context.
                // TODO: Add optional follow mode and structured event output.
                anyhow::bail!("run watch is not implemented yet")
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
    use serde_json::json;

    use super::{
        build_chunks,
        canonical_json,
        parse_structured_payload,
        read_inline_or_file,
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
}
