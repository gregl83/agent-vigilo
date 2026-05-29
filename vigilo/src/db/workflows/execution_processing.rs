//! Execution processing workflow.
//!
//! This module owns the worker-side path from a leased dataset case to persisted
//! evaluator results, execution aggregate, and terminal execution transition.
//! It also owns evaluator runtime lookup and single-flight component loading so
//! concurrent workers do not repeatedly compile the same evaluator artifact.

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    time::Instant,
};

use chrono::{
    DateTime,
    Utc,
};
use futures_util::{
    StreamExt,
    stream::FuturesUnordered,
};
use serde_json::json;
use sqlx::{
    PgPool,
    Postgres,
    QueryBuilder,
};
use tokio::task::{
    self,
    JoinSet,
};
use tracing::debug;
use uuid::Uuid;

use super::chunk_processing;
use crate::{
    agent_client,
    context::Context,
    contracts::{
        aggregation::{
            AggregationFinding,
            aggregate_findings,
            evaluation_status_key,
        },
        evaluator::{
            EvaluationDimension,
            EvaluationStatus,
            EvaluatorInput,
            Severity,
            TestCase,
        },
        evaluator_ref::parse_fully_qualified_evaluator,
        run::{
            AggregationSettings,
            CaseGroupProfile,
            EvaluatorBinding,
            RunProfile,
        },
    },
    db::tables::{
        evaluator_results,
        evaluators,
    },
    models::evaluator::EvaluatorState,
};

#[derive(Debug)]
struct EvaluatorExecutionRecord {
    evaluator_id: Uuid,
    status: EvaluationStatus,
    binding_dimension: String,
    source_dimension: Option<String>,
    normalized_score: Option<f64>,
    blocking: bool,
    binding_weight: f64,
    failure_category: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Clone)]
struct CaseEvaluationPlan {
    profile_group_id: String,
    evaluator_bindings: Vec<EvaluatorBinding>,
    aggregation: AggregationSettings,
}

/// Runtime metadata needed to execute one evaluator binding for a run.
#[derive(Debug, Clone)]
pub(crate) struct RunEvaluatorCatalogEntry {
    pub(crate) evaluator_id: Uuid,
    pub(crate) evaluator_version: String,
    pub(crate) evaluator_interface_version: Option<String>,
    pub(crate) evaluator_runtime_version: Option<String>,
}

/// Lookup table keyed by fully qualified evaluator ref.
pub(crate) type RunEvaluatorCatalog = BTreeMap<String, RunEvaluatorCatalogEntry>;

const CASE_EXECUTION_PARALLELISM: usize = 8;
const EVALUATOR_EXECUTION_PARALLELISM: usize = 8;
const EXECUTION_RETRY_BASE_SECONDS: i32 = 5;
const EXECUTION_RETRY_MAX_SECONDS: i32 = 600;

/// Terminal state transition to apply after evaluator processing finishes.
///
/// The attempt id and attempt number are authority tokens. A transition only
/// applies if the execution still points at the same current attempt.
#[derive(Debug, Clone)]
pub(crate) struct ExecutionTerminalTransition {
    pub(crate) execution_id: Uuid,
    pub(crate) attempt_id: Uuid,
    pub(crate) attempt_no: i32,
    pub(crate) completed: bool,
    pub(crate) error_message: Option<String>,
}

/// Result of processing a single case execution.
#[derive(Debug)]
pub(crate) struct ProcessedExecution {
    pub(crate) execution_id: Uuid,
    pub(crate) attempt_id: Uuid,
    pub(crate) result_count: usize,
    pub(crate) terminal_transition: ExecutionTerminalTransition,
}

#[derive(Debug, sqlx::FromRow)]
struct AttemptAllocation {
    case_id: Uuid,
    execution_id: Uuid,
    attempt_id: Option<Uuid>,
    attempt_no: i32,
    should_process: bool,
    already_terminal: bool,
    retry_not_due: bool,
    max_attempts_exhausted: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct ChunkExecutionState {
    pub(crate) open_execution_count: i64,
    pub(crate) retry_scheduled_count: i64,
    pub(crate) next_retry_after: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct CompletedExecutionPersistence {
    execution_id: Uuid,
    attempt_id: Uuid,
    attempt_no: i32,
    result_rows: Vec<evaluator_results::EvaluatorResultInsertRow>,
    overall_status: String,
    aggregate_score: Option<f64>,
    evaluator_result_count: i32,
    dimension_scores: serde_json::Value,
    blocking_failures: serde_json::Value,
    summary: serde_json::Value,
}

#[derive(Debug)]
struct CaseExecutionOutcome {
    processed: Option<ProcessedExecution>,
    persistence: Option<CompletedExecutionPersistence>,
}

#[derive(Debug, Default)]
struct BatchPersistenceStats {
    evaluator_results_attempted: usize,
    evaluator_results_inserted: u64,
    evaluator_result_conflicts: usize,
}

fn processed_terminal_failure(
    execution_id: Uuid,
    attempt_id: Uuid,
    attempt_no: i32,
    error_message: String,
) -> ProcessedExecution {
    ProcessedExecution {
        execution_id,
        attempt_id,
        result_count: 0,
        terminal_transition: ExecutionTerminalTransition {
            execution_id,
            attempt_id,
            attempt_no,
            completed: false,
            error_message: Some(error_message),
        },
    }
}

/// Extracts unique evaluator refs from a run profile.
///
/// Database behavior: none. This is a deterministic profile scan used before
/// catalog lookup so each evaluator identity is fetched once per run context.
pub(crate) fn evaluator_refs_from_profile(profile: &RunProfile) -> anyhow::Result<Vec<String>> {
    let mut unique_refs = BTreeSet::new();
    for group in &profile.case_groups {
        for binding in &group.evaluators {
            unique_refs.insert(binding.evaluator_ref.clone());
        }
    }

    Ok(unique_refs.into_iter().collect())
}

/// Builds the evaluator runtime catalog for a run profile in one database round trip.
///
/// The catalog validates that every referenced evaluator exists and is in a
/// runnable state before workers begin processing cases.
///
/// Query behavior:
/// - Parse each fully qualified evaluator ref from the profile.
/// - Fetch runtime metadata for the unique identities in one table query.
/// - Return a map keyed by the original ref string so execution can resolve
///   bindings without repeatedly querying evaluator rows.
pub(crate) async fn build_run_evaluator_catalog(
    db: &PgPool,
    profile: &RunProfile,
) -> anyhow::Result<RunEvaluatorCatalog> {
    let evaluator_refs = evaluator_refs_from_profile(profile)?;
    let parsed = evaluator_refs
        .iter()
        .map(|evaluator_ref| {
            parse_fully_qualified_evaluator(evaluator_ref)
                .map(|identity| (evaluator_ref.clone(), identity))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let identities = parsed
        .iter()
        .map(|(_, identity)| {
            (
                identity.namespace.clone(),
                identity.name.clone(),
                identity.version.clone(),
            )
        })
        .collect::<Vec<_>>();

    let fetched =
        evaluators::select_evaluator_runtime_metadata_by_identities(db, &identities).await?;
    let fetched_by_identity = fetched
        .into_iter()
        .map(|row| {
            let key = (row.namespace.clone(), row.name.clone(), row.version.clone());
            (key, row)
        })
        .collect::<BTreeMap<_, _>>();

    let mut catalog = BTreeMap::new();
    for (evaluator_ref, identity) in parsed {
        let key = (
            identity.namespace.clone(),
            identity.name.clone(),
            identity.version.clone(),
        );

        let Some(evaluator) = fetched_by_identity.get(&key) else {
            anyhow::bail!("evaluator '{}' no longer exists", evaluator_ref);
        };

        if !is_runnable_evaluator_state(&evaluator.state) {
            anyhow::bail!(
                "evaluator '{}' is not runnable in state '{:?}'",
                evaluator_ref,
                evaluator.state
            );
        }

        catalog.insert(
            evaluator_ref,
            RunEvaluatorCatalogEntry {
                evaluator_id: evaluator.id,
                evaluator_version: evaluator.version.clone(),
                evaluator_interface_version: evaluator.interface_version.clone(),
                evaluator_runtime_version: Some(evaluator.runtime_version.clone()),
            },
        );
    }

    Ok(catalog)
}

/// Loads or returns a cached Wasmtime component for a fully qualified evaluator ref.
///
/// Component compilation is single-flight through the registry cache, so
/// concurrent requests for the same evaluator share one load/compile operation.
///
/// Query behavior: on a cache miss, fetch the evaluator registry row, validate
/// that it is still runnable, compile the stored WASM bytes, and cache the
/// compiled component for the process.
pub(crate) async fn get_or_load_component(
    context: &Context,
    evaluator_ref: &str,
) -> anyhow::Result<wasmtime::component::Component> {
    let context = context.clone();
    let evaluator_ref_owned = evaluator_ref.to_string();
    let evaluator_ref_for_closure = evaluator_ref_owned.clone();
    let cache = context.reg().await?.clone();
    let component = cache
        .try_get_with(evaluator_ref_owned.clone(), async move {
            let identity = parse_fully_qualified_evaluator(&evaluator_ref_for_closure)?;
            let db = context.db().await?;
            let evaluator_record = evaluators::select_evaluator(
                db,
                &identity.namespace,
                &identity.name,
                &identity.version,
            )
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "evaluator '{}' was not found in registry during worker execution",
                    evaluator_ref_for_closure
                )
            })?;

            if !is_runnable_evaluator_state(&evaluator_record.state) {
                anyhow::bail!(
                    "evaluator '{}' is not runnable in state '{:?}'",
                    evaluator_ref_for_closure,
                    evaluator_record.state
                );
            }

            let wasm = context.wasm().await?;
            wasm.compile_component(&evaluator_record.wasm_bytes)
        })
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "single-flight component load failed for evaluator '{}': {}",
                evaluator_ref_owned,
                err
            )
        })?;

    Ok(component)
}

fn tags_from_case_row(tags: &serde_json::Value) -> Vec<String> {
    tags.as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect()
}

fn metadata_from_case_row(
    metadata: &serde_json::Value,
) -> anyhow::Result<BTreeMap<String, serde_json::Value>> {
    match metadata {
        serde_json::Value::Object(map) => Ok(map
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()),
        _ => anyhow::bail!("case metadata must be a JSON object"),
    }
}

fn matching_groups_for_case<'a>(
    profile: &'a RunProfile,
    case: &chunk_processing::WorkerCaseBatchItem,
) -> Vec<&'a CaseGroupProfile> {
    if let Some(group_id) = case.case_group.as_deref() {
        return profile
            .case_groups
            .iter()
            .filter(|group| group.id == group_id)
            .collect();
    }

    let case_tags = tags_from_case_row(&case.tags);

    profile
        .case_groups
        .iter()
        .filter(|group| {
            if group.applies_to.task_type != case.task_type {
                return false;
            }

            if !group.applies_to.tags_any.is_empty()
                && !group
                    .applies_to
                    .tags_any
                    .iter()
                    .any(|tag| case_tags.iter().any(|case_tag| case_tag == tag))
            {
                return false;
            }

            if !group
                .applies_to
                .tags_all
                .iter()
                .all(|tag| case_tags.iter().any(|case_tag| case_tag == tag))
            {
                return false;
            }

            true
        })
        .collect()
}

fn merge_aggregation_settings(
    case_id: Uuid,
    groups: &[&CaseGroupProfile],
) -> anyhow::Result<AggregationSettings> {
    let mut merged = AggregationSettings {
        dimensions: BTreeMap::new(),
    };

    for group in groups {
        for (dimension, policy) in &group.aggregation.dimensions {
            if let Some(existing) = merged.dimensions.get(dimension) {
                if existing != policy {
                    anyhow::bail!(
                        "case '{}' matched conflicting aggregation policy for dimension '{}' across case_groups",
                        case_id,
                        dimension
                    );
                }
                continue;
            }

            merged.dimensions.insert(dimension.clone(), policy.clone());
        }
    }

    Ok(merged)
}

fn equivalent_evaluator_binding(left: &EvaluatorBinding, right: &EvaluatorBinding) -> bool {
    left.dimension == right.dimension
        && left.blocking == right.blocking
        && left.weight == right.weight
        && left.config == right.config
}

/// Resolves evaluator bindings and aggregation policy for one dataset case.
///
/// Database behavior: none. Runtime aggregation is driven by the profile
/// case-group policies that selected the evaluator bindings. When multiple
/// groups match the same case, non-overlapping or identical dimension policies
/// are merged; conflicting policies are rejected rather than guessed.
fn evaluation_plan_for_case(
    profile: &RunProfile,
    case: &chunk_processing::WorkerCaseBatchItem,
) -> anyhow::Result<CaseEvaluationPlan> {
    let matching_groups = matching_groups_for_case(profile, case);
    if matching_groups.is_empty() {
        anyhow::bail!(
            "case '{}' did not match any evaluator bindings in run profile",
            case.case_id
        );
    }

    let profile_group_id = matching_groups
        .iter()
        .map(|group| group.id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let aggregation = merge_aggregation_settings(case.case_id, &matching_groups)?;
    let mut unique = BTreeMap::new();
    for group in matching_groups {
        for binding in &group.evaluators {
            if let Some(existing) = unique.get(&binding.evaluator_ref) {
                if !equivalent_evaluator_binding(existing, binding) {
                    anyhow::bail!(
                        "case '{}' matched conflicting evaluator binding for '{}' across case_groups",
                        case.case_id,
                        binding.evaluator_ref
                    );
                }
                continue;
            }

            unique.insert(binding.evaluator_ref.clone(), binding.clone());
        }
    }

    let evaluator_bindings = unique.into_values().collect::<Vec<_>>();
    if evaluator_bindings.is_empty() {
        anyhow::bail!(
            "case '{}' matched run profile groups with no evaluator bindings",
            case.case_id
        );
    }

    Ok(CaseEvaluationPlan {
        profile_group_id,
        evaluator_bindings,
        aggregation,
    })
}

/// Converts a persisted chunk case row into the evaluator contract shape.
///
/// Database behavior: none. This is the boundary where stored payloads become
/// WIT-facing `input`, `expected`, `context`, tag, and metadata fields.
fn make_test_case(case: &chunk_processing::WorkerCaseBatchItem) -> anyhow::Result<TestCase> {
    Ok(TestCase {
        id: case.case_id.to_string(),
        task_type: case.task_type.clone(),
        case_group: case.case_group.clone(),
        input: case.input_payload.clone(),
        expected: Some(case.expected_output.clone()),
        context: Some(case.context_payload.clone()),
        tags: tags_from_case_row(&case.tags),
        metadata: metadata_from_case_row(&case.metadata)?,
    })
}

/// Allocates durable execution attempts for a chunk case batch.
///
/// Query behavior:
/// - Materializes the input cases as an inline table to preserve chunk order.
/// - Takes a shared run-state guard so workers for other chunks can continue,
///   while cancellation/finalization still waits for this short write.
/// - Upserts execution rows without resetting durable retry or terminal state.
/// - Splits rows into terminal, retry-waiting, exhausted-open, and retry-eligible
///   buckets.
/// - For eligible rows, marks older running attempts stale, increments the
///   execution attempt number, inserts a new running attempt, and stores it as
///   the current authoritative attempt.
///
/// The returned flags tell the worker whether a case should run now, wait for
/// `retry_after`, be skipped because it is already terminal, or be failed
/// because its retry budget is exhausted.
async fn allocate_execution_attempts_for_cases(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    chunk_id: Uuid,
    run_profile: &RunProfile,
    cases: &[chunk_processing::WorkerCaseBatchItem],
    evaluation_plans_by_case: &[CaseEvaluationPlan],
) -> anyhow::Result<Vec<AttemptAllocation>> {
    if cases.is_empty() {
        return Ok(Vec::new());
    }
    if run_profile.defaults.max_attempts == 0 {
        anyhow::bail!("run profile defaults.max_attempts must be greater than zero");
    }
    let max_attempts = i32::try_from(run_profile.defaults.max_attempts)?;

    if cases.len() != evaluation_plans_by_case.len() {
        anyhow::bail!(
            "case batch has {} cases but {} evaluation plans",
            cases.len(),
            evaluation_plans_by_case.len()
        );
    }

    struct AllocationInput<'a> {
        case: &'a chunk_processing::WorkerCaseBatchItem,
        profile_group_id: &'a str,
        evaluator_manifest: serde_json::Value,
        expected_evaluator_count: i32,
        input_ordinal: i32,
    }

    let inputs = cases
        .iter()
        .zip(evaluation_plans_by_case)
        .enumerate()
        .map(|(index, (case, plan))| {
            Ok(AllocationInput {
                case,
                profile_group_id: &plan.profile_group_id,
                evaluator_manifest: serde_json::to_value(&plan.evaluator_bindings)?,
                expected_evaluator_count: i32::try_from(plan.evaluator_bindings.len())?,
                input_ordinal: i32::try_from(index)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    // Query outline:
    //
    // input             - chunk cases and resolved evaluator manifests.
    // run_guard         - shared lifecycle check for the running run.
    // attempt_policy    - max_attempts copied into SQL for retry decisions.
    // upserted          - create/update execution rows without clearing retry state.
    // attempt_state     - durable state after the upsert.
    // terminal_or_closed - already terminal rows, skipped by worker.
    // retry_waiting     - retry_scheduled rows whose retry_after is not due.
    // exhausted_open    - open rows at max_attempts, failed by caller.
    // retry_eligible    - rows that should receive a new attempt now.
    // superseded_attempts/bumped/inserted_attempt/updated_execution
    //                   - authority handoff to the new running attempt.
    let mut query_builder = QueryBuilder::<Postgres>::new(
        r#"
        WITH input (
            case_id,
            case_hash,
            task_type,
            tags,
            input_payload,
            expected_output,
            case_metadata,
            profile_group_id,
            evaluator_manifest,
            expected_evaluator_count,
            input_ordinal
        ) AS (
        "#,
    );

    query_builder.push_values(inputs.iter(), |mut b, row| {
        b.push_bind(&row.case.case_id)
            .push_bind(&row.case.case_hash)
            .push_bind(&row.case.task_type)
            .push_bind(&row.case.tags)
            .push_bind(&row.case.input_payload)
            .push_bind(&row.case.expected_output)
            .push_bind(&row.case.metadata)
            .push_bind(row.profile_group_id)
            .push_bind(&row.evaluator_manifest)
            .push_bind(row.expected_evaluator_count)
            .push_bind(row.input_ordinal);
    });

    query_builder.push(
        r#"
        ),
        run_guard AS (
            SELECT id
            FROM runs
            WHERE id =
        "#,
    );
    query_builder.push_bind(run_id);
    query_builder.push(
        r#"::uuid
              AND status = 'running'::run_status
            FOR SHARE
        ),
        attempt_policy AS (
            SELECT
        "#,
    );
    query_builder.push_bind(max_attempts);
    query_builder.push(
        r#"::int AS max_attempts
        ),
        upserted AS (
            INSERT INTO executions (
                run_id,
                run_shard,
                chunk_id,
                case_id,
                case_hash,
                profile_group_id,
                task_type,
                tags,
                input_payload,
                expected_output,
                case_metadata,
                evaluation_profile_id,
                evaluation_profile_version,
                evaluator_manifest,
                expected_evaluator_count,
                status,
                started_at,
                updated_at
            )
            SELECT
                "#,
    );
    query_builder.push_bind(run_id);
    query_builder.push(
        r#"::uuid,
                "#,
    );
    query_builder.push_bind(run_shard);
    query_builder.push(
        r#",
                "#,
    );
    query_builder.push_bind(chunk_id);
    query_builder.push(
        r#"::uuid,
                input.case_id,
                input.case_hash,
                input.profile_group_id,
                input.task_type,
                input.tags::jsonb,
                input.input_payload::jsonb,
                input.expected_output::jsonb,
                input.case_metadata::jsonb,
                "#,
    );
    query_builder.push_bind(&run_profile.profile_id);
    query_builder.push(",");
    query_builder.push_bind(&run_profile.profile_version);
    query_builder.push(
        r#",
                input.evaluator_manifest::jsonb,
                input.expected_evaluator_count,
                'pending'::execution_status,
                NULL,
                now()
            FROM input
            JOIN run_guard
              ON true
            ON CONFLICT (run_id, run_shard, case_id) DO UPDATE
            SET case_hash = EXCLUDED.case_hash,
                profile_group_id = EXCLUDED.profile_group_id,
                task_type = EXCLUDED.task_type,
                tags = EXCLUDED.tags,
                input_payload = EXCLUDED.input_payload,
                expected_output = EXCLUDED.expected_output,
                case_metadata = EXCLUDED.case_metadata,
                evaluation_profile_id = EXCLUDED.evaluation_profile_id,
                evaluation_profile_version = EXCLUDED.evaluation_profile_version,
                evaluator_manifest = EXCLUDED.evaluator_manifest,
                expected_evaluator_count = EXCLUDED.expected_evaluator_count,
                updated_at = now()
            RETURNING id, case_id, run_id, run_shard
        ),
        attempt_state AS (
            SELECT
                input.case_id,
                input.input_ordinal,
                upserted.id AS execution_id,
                upserted.run_id,
                upserted.run_shard,
                executions.status,
                executions.current_attempt_id,
                executions.current_attempt_no,
                executions.retry_after
            FROM upserted
            JOIN executions
              ON executions.run_id = upserted.run_id
             AND executions.run_shard = upserted.run_shard
             AND executions.id = upserted.id
            JOIN input
              ON input.case_id = upserted.case_id
        ),
        terminal_or_closed AS (
            SELECT
                attempt_state.case_id,
                attempt_state.execution_id,
                attempt_state.current_attempt_id AS attempt_id,
                attempt_state.current_attempt_no AS attempt_no,
                true AS already_terminal,
                false AS retry_not_due,
                false AS max_attempts_exhausted,
                attempt_state.input_ordinal
            FROM attempt_state, attempt_policy
            WHERE attempt_state.status IN (
                    'completed'::execution_status,
                    'failed'::execution_status,
                    'timed_out'::execution_status,
                    'cancelled'::execution_status
                )
        ),
        retry_waiting AS (
            SELECT
                attempt_state.case_id,
                attempt_state.execution_id,
                attempt_state.current_attempt_id AS attempt_id,
                attempt_state.current_attempt_no AS attempt_no,
                false AS already_terminal,
                true AS retry_not_due,
                false AS max_attempts_exhausted,
                attempt_state.input_ordinal
            FROM attempt_state, attempt_policy
            WHERE attempt_state.status = 'retry_scheduled'::execution_status
              AND attempt_state.current_attempt_no < attempt_policy.max_attempts
              AND attempt_state.retry_after > now()
        ),
        exhausted_open AS (
            SELECT
                attempt_state.case_id,
                attempt_state.execution_id,
                attempt_state.current_attempt_id AS attempt_id,
                attempt_state.current_attempt_no AS attempt_no,
                false AS already_terminal,
                false AS retry_not_due,
                true AS max_attempts_exhausted,
                attempt_state.input_ordinal
            FROM attempt_state, attempt_policy
            WHERE attempt_state.status IN (
                    'pending'::execution_status,
                    'running'::execution_status,
                    'awaiting_evaluators'::execution_status,
                    'retry_scheduled'::execution_status
                )
              AND attempt_state.current_attempt_no >= attempt_policy.max_attempts
              AND attempt_state.current_attempt_id IS NOT NULL
        ),
        retry_eligible AS (
            SELECT
                attempt_state.case_id,
                attempt_state.input_ordinal,
                attempt_state.execution_id AS id,
                attempt_state.run_id,
                attempt_state.run_shard
            FROM attempt_state, attempt_policy
            WHERE attempt_state.status IN (
                    'pending'::execution_status,
                    'running'::execution_status,
                    'awaiting_evaluators'::execution_status,
                    'retry_scheduled'::execution_status
                )
              AND attempt_state.current_attempt_no < attempt_policy.max_attempts
              AND (
                    attempt_state.status <> 'retry_scheduled'::execution_status
                    OR attempt_state.retry_after IS NULL
                    OR attempt_state.retry_after <= now()
              )
        ),
        superseded_attempts AS (
            UPDATE execution_attempts
            SET status = 'stale'::attempt_status,
                error_message = COALESCE(
                    execution_attempts.error_message,
                    'attempt superseded by a newer worker attempt'
                ),
                completed_at = COALESCE(execution_attempts.completed_at, now()),
                updated_at = now()
            FROM retry_eligible
            WHERE execution_attempts.run_id = retry_eligible.run_id
              AND execution_attempts.run_shard = retry_eligible.run_shard
              AND execution_attempts.execution_id = retry_eligible.id
              AND execution_attempts.status = 'running'::attempt_status
            RETURNING execution_attempts.id
        ),
        bumped AS (
            UPDATE executions
            SET status = 'running'::execution_status,
                current_attempt_no = executions.current_attempt_no + 1,
                last_error_message = NULL,
                retry_after = NULL,
                started_at = COALESCE(executions.started_at, now()),
                completed_at = NULL,
                updated_at = now()
            FROM retry_eligible
            WHERE executions.run_id = retry_eligible.run_id
              AND executions.run_shard = retry_eligible.run_shard
              AND executions.id = retry_eligible.id
            RETURNING
                executions.id AS execution_id,
                executions.run_id,
                executions.run_shard,
                executions.current_attempt_no AS attempt_no
        ),
        inserted_attempt AS (
            INSERT INTO execution_attempts (
                execution_id,
                run_id,
                run_shard,
                attempt_no,
                status,
                started_at,
                created_at,
                updated_at
            )
            SELECT
                bumped.execution_id,
                bumped.run_id,
                bumped.run_shard,
                bumped.attempt_no,
                'running'::attempt_status,
                now(),
                now(),
                now()
            FROM bumped
            RETURNING id AS attempt_id, execution_id, run_id, run_shard, attempt_no
        ),
        updated_execution AS (
            UPDATE executions
            SET current_attempt_id = inserted_attempt.attempt_id
            FROM inserted_attempt
            WHERE executions.run_id = inserted_attempt.run_id
              AND executions.run_shard = inserted_attempt.run_shard
              AND executions.id = inserted_attempt.execution_id
            RETURNING executions.id
        ),
        allocated AS (
            SELECT
                retry_eligible.case_id,
                inserted_attempt.execution_id,
                inserted_attempt.attempt_id,
                inserted_attempt.attempt_no,
                true AS should_process,
                false AS already_terminal,
                false AS retry_not_due,
                false AS max_attempts_exhausted,
                retry_eligible.input_ordinal
            FROM inserted_attempt
            JOIN retry_eligible
              ON retry_eligible.id = inserted_attempt.execution_id
            JOIN updated_execution
              ON updated_execution.id = inserted_attempt.execution_id
            UNION ALL
            SELECT
                terminal_or_closed.case_id,
                terminal_or_closed.execution_id,
                terminal_or_closed.attempt_id,
                terminal_or_closed.attempt_no,
                false AS should_process,
                terminal_or_closed.already_terminal,
                terminal_or_closed.retry_not_due,
                terminal_or_closed.max_attempts_exhausted,
                terminal_or_closed.input_ordinal
            FROM terminal_or_closed
            UNION ALL
            SELECT
                retry_waiting.case_id,
                retry_waiting.execution_id,
                retry_waiting.attempt_id,
                retry_waiting.attempt_no,
                false AS should_process,
                retry_waiting.already_terminal,
                retry_waiting.retry_not_due,
                retry_waiting.max_attempts_exhausted,
                retry_waiting.input_ordinal
            FROM retry_waiting
            UNION ALL
            SELECT
                exhausted_open.case_id,
                exhausted_open.execution_id,
                exhausted_open.attempt_id,
                exhausted_open.attempt_no,
                false AS should_process,
                exhausted_open.already_terminal,
                exhausted_open.retry_not_due,
                exhausted_open.max_attempts_exhausted,
                exhausted_open.input_ordinal
            FROM exhausted_open
        )
        SELECT
            allocated.case_id,
            allocated.execution_id,
            allocated.attempt_id,
            allocated.attempt_no,
            allocated.should_process,
            allocated.already_terminal,
            allocated.retry_not_due,
            allocated.max_attempts_exhausted
        FROM allocated
        ORDER BY allocated.input_ordinal
        "#,
    );

    let allocations = query_builder
        .build_query_as::<AttemptAllocation>()
        .fetch_all(db)
        .await?;

    if allocations.len() != cases.len() {
        anyhow::bail!(
            "allocated {} execution attempts for {} cases",
            allocations.len(),
            cases.len()
        );
    }

    Ok(allocations)
}

/// Applies terminal execution transitions as one authoritative batch.
///
/// The batch updates attempts and executions together. If any transition no
/// longer owns the current attempt, the entire batch is rejected so stale
/// workers cannot mark executions terminal. The run-state guard is shared so
/// other chunks for the same running run are not serialized on the run row.
///
/// Query behavior:
/// - Unnests the worker's transition batch into a relational input set.
/// - Re-checks the run is still `running` and every transition still owns the
///   execution's current attempt id and attempt number.
/// - Requires completed transitions to have an aggregate for the same attempt.
/// - Marks failed attempts retryable when `attempt_no < max_attempts`, using a
///   bounded exponential `retry_after`.
/// - Writes terminal failed aggregates only when retry budget is exhausted.
pub(crate) async fn finalize_execution_terminal_transitions(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    max_attempts: i32,
    transitions: &[ExecutionTerminalTransition],
) -> anyhow::Result<()> {
    if transitions.is_empty() {
        return Ok(());
    }

    let execution_ids = transitions
        .iter()
        .map(|transition| transition.execution_id)
        .collect::<Vec<_>>();
    let attempt_ids = transitions
        .iter()
        .map(|transition| transition.attempt_id)
        .collect::<Vec<_>>();
    let attempt_nos = transitions
        .iter()
        .map(|transition| transition.attempt_no)
        .collect::<Vec<_>>();
    let completed_flags = transitions
        .iter()
        .map(|transition| transition.completed)
        .collect::<Vec<_>>();
    let error_messages = transitions
        .iter()
        .map(|transition| transition.error_message.clone())
        .collect::<Vec<_>>();

    // Query outline:
    //
    // transition_input       - worker-produced terminal transitions.
    // run_guard             - shared check that the run is still running.
    // authoritative_input   - transitions that still own current_attempt_*.
    // authority_check       - all-or-nothing stale worker guard.
    // terminal_input        - requires aggregates for completed attempts.
    // attempt_update        - marks attempts completed/failed and computes retry.
    // failed_aggregate_upsert
    //                       - creates final error aggregate only after retries end.
    // execution_update      - moves executions to completed/retry_scheduled/failed.
    let applied = sqlx::query_scalar::<_, i64>(
        r#"
        WITH transition_input AS (
            SELECT *
            FROM UNNEST(
                $1::uuid[],
                $2::uuid[],
                $3::int4[],
                $4::bool[],
                $5::text[]
            ) AS t(execution_id, attempt_id, attempt_no, completed, error_message)
        ),
        transition_count AS (
            SELECT COUNT(*) AS expected_count
            FROM transition_input
        ),
        run_guard AS (
            SELECT id
            FROM runs
            WHERE id = $6::uuid
              AND status = 'running'::run_status
            FOR SHARE
        ),
        authoritative_input AS (
            SELECT
                transition_input.*,
                executions.run_id,
                executions.run_shard
            FROM transition_input
            JOIN executions
              ON executions.run_id = $6::uuid
             AND executions.run_shard = $7
             AND executions.id = transition_input.execution_id
             AND executions.current_attempt_id = transition_input.attempt_id
             AND executions.current_attempt_no = transition_input.attempt_no
            JOIN run_guard
              ON run_guard.id = executions.run_id
        ),
        authority_check AS (
            SELECT transition_count.expected_count
            FROM transition_count
            WHERE transition_count.expected_count = (
                SELECT COUNT(*)
                FROM authoritative_input
            )
        ),
        terminal_input AS (
            SELECT
                authoritative_input.*,
                CASE
                    WHEN authoritative_input.completed THEN execution_aggregates.overall_status
                    ELSE 'error'::evaluation_status
                END AS overall_status
            FROM authoritative_input
            LEFT JOIN execution_aggregates
              ON execution_aggregates.run_id = $6::uuid
             AND execution_aggregates.run_shard = $7
             AND execution_aggregates.execution_id = authoritative_input.execution_id
             AND execution_aggregates.attempt_id = authoritative_input.attempt_id
            WHERE NOT authoritative_input.completed
               OR execution_aggregates.execution_id IS NOT NULL
        ),
        terminal_input_check AS (
            SELECT transition_count.expected_count
            FROM transition_count, authority_check
            WHERE transition_count.expected_count = (
                SELECT COUNT(*)
                FROM terminal_input
            )
        ),
        stale_attempt_update AS (
            UPDATE execution_attempts
            SET status = 'stale'::attempt_status,
                error_message = COALESCE(
                    transition_input.error_message,
                    'attempt lost authority before terminal transition'
                ),
                completed_at = now(),
                updated_at = now()
            FROM transition_input
            JOIN executions
              ON executions.run_id = $6::uuid
             AND executions.run_shard = $7
             AND executions.id = transition_input.execution_id
            WHERE execution_attempts.run_id = $6::uuid
              AND execution_attempts.run_shard = $7
              AND execution_attempts.id = transition_input.attempt_id
              AND execution_attempts.execution_id = transition_input.execution_id
              AND execution_attempts.status = 'running'::attempt_status
              AND (
                  executions.current_attempt_id IS DISTINCT FROM transition_input.attempt_id
                  OR executions.current_attempt_no IS DISTINCT FROM transition_input.attempt_no
              )
            RETURNING execution_attempts.id
        ),
        attempt_update AS (
            UPDATE execution_attempts
            SET status = CASE
                    WHEN terminal_input.completed THEN 'completed'::attempt_status
                    WHEN terminal_input.error_message LIKE 'agent invocation failed:%'
                        THEN 'failed_agent_call'::attempt_status
                    ELSE 'failed_evaluation'::attempt_status
                END,
                error_message = CASE
                    WHEN terminal_input.completed THEN NULL
                    ELSE terminal_input.error_message
                END,
                completed_at = now(),
                updated_at = now()
            FROM terminal_input, terminal_input_check
            WHERE execution_attempts.run_id = $6::uuid
              AND execution_attempts.run_shard = $7
              AND execution_attempts.id = terminal_input.attempt_id
              AND execution_attempts.execution_id = terminal_input.execution_id
            RETURNING
                terminal_input.execution_id,
                terminal_input.run_id,
                terminal_input.run_shard,
                terminal_input.attempt_id,
                terminal_input.attempt_no,
                terminal_input.completed,
                terminal_input.error_message,
                terminal_input.overall_status,
                (
                    NOT terminal_input.completed
                    AND terminal_input.attempt_no < $8::int
                ) AS retry_scheduled
        ),
        failed_aggregate_upsert AS (
            INSERT INTO execution_aggregates (
                execution_id,
                run_id,
                run_shard,
                attempt_id,
                overall_status,
                aggregate_score,
                evaluator_result_count,
                dimension_scores,
                blocking_failures,
                summary,
                updated_at
            )
            SELECT
                attempt_update.execution_id,
                attempt_update.run_id,
                attempt_update.run_shard,
                attempt_update.attempt_id,
                'error'::evaluation_status,
                NULL,
                0,
                '{}'::jsonb,
                jsonb_build_array(jsonb_build_object(
                    'status', 'error',
                    'reason', attempt_update.error_message
                )),
                jsonb_build_object(
                    'attempt_id', attempt_update.attempt_id,
                    'result_count', 0,
                    'overall_status', 'error',
                    'error_message', attempt_update.error_message
                ),
                now()
            FROM attempt_update
            WHERE NOT attempt_update.completed
              AND NOT attempt_update.retry_scheduled
            ON CONFLICT (run_id, run_shard, execution_id) DO UPDATE
            SET attempt_id = EXCLUDED.attempt_id,
                overall_status = EXCLUDED.overall_status,
                aggregate_score = EXCLUDED.aggregate_score,
                evaluator_result_count = EXCLUDED.evaluator_result_count,
                dimension_scores = EXCLUDED.dimension_scores,
                blocking_failures = EXCLUDED.blocking_failures,
                summary = EXCLUDED.summary,
                updated_at = now()
            RETURNING execution_id
        ),
        execution_update AS (
            UPDATE executions
            SET status = CASE
                    WHEN attempt_update.completed THEN 'completed'::execution_status
                    WHEN attempt_update.retry_scheduled THEN 'retry_scheduled'::execution_status
                    ELSE 'failed'::execution_status
                END,
                current_attempt_no = attempt_update.attempt_no,
                current_attempt_id = attempt_update.attempt_id,
                last_error_message = CASE
                    WHEN attempt_update.completed THEN NULL
                    ELSE attempt_update.error_message
                END,
                retry_after = CASE
                    WHEN attempt_update.retry_scheduled THEN
                        now() + (
                            LEAST(
                                $9::int * POWER(3::numeric, GREATEST(attempt_update.attempt_no - 1, 0)),
                                $10::int::numeric
                            )::int * interval '1 second'
                        )
                    ELSE NULL
                END,
                retry_count = CASE
                    WHEN attempt_update.retry_scheduled THEN executions.retry_count + 1
                    ELSE executions.retry_count
                END,
                last_attempt_completed_at = now(),
                completed_at = CASE
                    WHEN attempt_update.retry_scheduled THEN NULL
                    ELSE now()
                END,
                updated_at = now()
            FROM attempt_update
            LEFT JOIN failed_aggregate_upsert
              ON failed_aggregate_upsert.execution_id = attempt_update.execution_id
            WHERE executions.run_id = $6::uuid
              AND executions.run_shard = $7
              AND executions.id = attempt_update.execution_id
              AND executions.current_attempt_id = attempt_update.attempt_id
              AND executions.current_attempt_no = attempt_update.attempt_no
            RETURNING
                executions.id AS execution_id,
                executions.run_id,
                attempt_update.attempt_id,
                attempt_update.overall_status
        )
        SELECT
            (SELECT COUNT(*)::bigint FROM execution_update)
        "#,
    )
    .bind(execution_ids)
    .bind(attempt_ids)
    .bind(attempt_nos)
    .bind(completed_flags)
    .bind(error_messages)
    .bind(run_id)
    .bind(run_shard)
    .bind(max_attempts)
    .bind(EXECUTION_RETRY_BASE_SECONDS)
    .bind(EXECUTION_RETRY_MAX_SECONDS)
    .fetch_one(db)
    .await?;

    let expected = u64::try_from(transitions.len())?;
    if u64::try_from(applied)? != expected {
        anyhow::bail!(
            "terminal transition batch applied {} current executions out of {}; at least one attempt lost authority or completed without an aggregate",
            applied,
            expected
        );
    }

    Ok(())
}

/// Summarizes whether a chunk's executions are terminal or still waiting for retry.
///
/// Query behavior: counts open execution statuses for the chunk's case ids and
/// returns the earliest retry window. The worker uses this after terminal
/// transitions to decide whether to complete the chunk or release it and delay
/// the message until retry work is due.
pub(crate) async fn summarize_chunk_execution_state(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    cases: &[chunk_processing::WorkerCaseBatchItem],
) -> anyhow::Result<ChunkExecutionState> {
    if cases.is_empty() {
        return Ok(ChunkExecutionState {
            open_execution_count: 0,
            retry_scheduled_count: 0,
            next_retry_after: None,
        });
    }

    let case_ids = cases.iter().map(|case| case.case_id).collect::<Vec<_>>();
    let state = sqlx::query_as::<_, ChunkExecutionState>(
        r#"
        SELECT
            COUNT(*) FILTER (
                WHERE status IN (
                    'pending'::execution_status,
                    'running'::execution_status,
                    'awaiting_evaluators'::execution_status,
                    'retry_scheduled'::execution_status
                )
            )::bigint AS open_execution_count,
            COUNT(*) FILTER (
                WHERE status = 'retry_scheduled'::execution_status
            )::bigint AS retry_scheduled_count,
            MIN(retry_after) FILTER (
                WHERE status = 'retry_scheduled'::execution_status
            ) AS next_retry_after
        FROM executions
        WHERE run_id = $1::uuid
          AND run_shard = $2
          AND case_id = ANY($3::uuid[])
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(&case_ids)
    .fetch_one(db)
    .await?;

    Ok(state)
}

fn map_evaluation_status(status: &EvaluationStatus) -> &'static str {
    evaluation_status_key(status)
}

fn map_evaluation_dimension(dimension: &EvaluationDimension) -> String {
    match dimension {
        EvaluationDimension::Correctness => "correctness".to_string(),
        EvaluationDimension::Format => "format".to_string(),
        EvaluationDimension::Safety => "safety".to_string(),
        EvaluationDimension::Quality => "quality".to_string(),
        EvaluationDimension::Latency => "latency".to_string(),
        EvaluationDimension::ToolUse => "tool_use".to_string(),
        EvaluationDimension::Calibration => "calibration".to_string(),
        EvaluationDimension::Other(value) => value.clone(),
    }
}

fn map_severity(severity: &Severity) -> &'static str {
    match severity {
        Severity::None => "none",
        Severity::Low => "low",
        Severity::Medium => "medium",
        Severity::High => "high",
        Severity::Critical => "critical",
    }
}

async fn evaluate_case_execution(
    context: &Context,
    run_id: Uuid,
    run_shard: i16,
    run_profile: &RunProfile,
    evaluator_catalog: &RunEvaluatorCatalog,
    case: &chunk_processing::WorkerCaseBatchItem,
    evaluator_bindings: &[EvaluatorBinding],
    aggregation: &AggregationSettings,
    allocation: &AttemptAllocation,
) -> CaseExecutionOutcome {
    let runtime_started = Instant::now();
    let execution_id = allocation.execution_id;
    let Some(attempt_id) = allocation.attempt_id else {
        let failure_message = format!(
            "case '{}' was selected for processing without an allocated attempt",
            case.case_id
        );
        return CaseExecutionOutcome {
            processed: Some(processed_terminal_failure(
                execution_id,
                Uuid::nil(),
                allocation.attempt_no,
                failure_message,
            )),
            persistence: None,
        };
    };
    let attempt_no = allocation.attempt_no;

    let evaluation_result: anyhow::Result<(
        Vec<EvaluatorExecutionRecord>,
        Vec<evaluator_results::EvaluatorResultInsertRow>,
    )> = async {
        let test_case = make_test_case(case)?;
        let http = context.http().await?;
        let agent_output = agent_client::invoke(
            http,
            run_id,
            execution_id,
            attempt_id,
            run_profile,
            &test_case,
        )
        .await
        .map_err(|err| anyhow::anyhow!("agent invocation failed: {}", err))?;

        let mut ordered_records = BTreeMap::new();
        let mut ordered_rows = BTreeMap::new();
        let parallelism = EVALUATOR_EXECUTION_PARALLELISM.min(evaluator_bindings.len().max(1));
        let profile_id = run_profile.profile_id.clone();
        let profile_version = run_profile.profile_version.clone();
        let mut next_index = 0usize;
        let mut tasks = JoinSet::<
            anyhow::Result<(
                usize,
                Vec<EvaluatorExecutionRecord>,
                Vec<evaluator_results::EvaluatorResultInsertRow>,
            )>,
        >::new();

        while next_index < evaluator_bindings.len() || !tasks.is_empty() {
            while next_index < evaluator_bindings.len() && tasks.len() < parallelism {
                let index = next_index;
                next_index += 1;

                let binding = evaluator_bindings[index].clone();
                let evaluator_entry = evaluator_catalog
                    .get(&binding.evaluator_ref)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "evaluator '{}' is missing from run evaluator catalog",
                            binding.evaluator_ref
                        )
                    })?;
                let dimension_policy_blocking = aggregation
                    .dimensions
                    .get(&binding.dimension)
                    .map(|policy| policy.blocking)
                    .unwrap_or(false);
                let context = context.clone();
                let test_case = test_case.clone();
                let agent_output = agent_output.clone();
                let profile_id = profile_id.clone();
                let profile_version = profile_version.clone();
                tasks.spawn(async move {
                    let wasm = context.wasm().await?.clone();
                    let component = get_or_load_component(&context, &binding.evaluator_ref).await?;
                    let wasm_permit = wasm.acquire_evaluation_permit().await?;

                    let evaluator_input = EvaluatorInput {
                        run_id: run_id.to_string(),
                        execution_id: execution_id.to_string(),
                        attempt_id: attempt_id.to_string(),
                        case: test_case,
                        actual: agent_output,
                        evaluator_config: binding.config.clone(),
                    };
                    let output = task::spawn_blocking(move || {
                        let _wasm_permit = wasm_permit;
                        wasm.test_evaluator_component(&component, evaluator_input)
                    })
                    .await
                    .map_err(|err| {
                        anyhow::anyhow!("evaluator blocking task join failed: {}", err)
                    })?;

                    let mut records = Vec::new();
                    let mut rows = Vec::new();

                    match output {
                        Ok(evaluator_output) => {
                            let serialized_output = serde_json::to_value(&evaluator_output)?;
                            let normalized = evaluator_output.normalize();

                            if normalized.is_empty() {
                                let status = EvaluationStatus::Error;
                                let failure_category = Some("empty_output".to_string());
                                let reason = Some("evaluator returned no findings".to_string());
                                rows.push(evaluator_results::EvaluatorResultInsertRow {
                                    run_id,
                                    run_shard,
                                    execution_id,
                                    attempt_id,
                                    evaluator_id: evaluator_entry.evaluator_id,
                                    finding_index: 0,
                                    evaluator_version: evaluator_entry.evaluator_version.clone(),
                                    evaluator_profile_id: profile_id.clone(),
                                    evaluator_profile_version: profile_version.clone(),
                                    evaluator_interface_version: evaluator_entry
                                        .evaluator_interface_version
                                        .clone(),
                                    evaluator_runtime_version: evaluator_entry
                                        .evaluator_runtime_version
                                        .clone(),
                                    dimension: binding.dimension.clone(),
                                    status: map_evaluation_status(&status).to_string(),
                                    blocking: binding.blocking || dimension_policy_blocking,
                                    score_kind: "informational".to_string(),
                                    raw_score: None,
                                    raw_score_min: None,
                                    raw_score_max: None,
                                    normalized_score: None,
                                    weight: binding.weight,
                                    severity: "high".to_string(),
                                    failure_category: failure_category.clone(),
                                    reason: reason.clone(),
                                    evidence: json!({}),
                                    raw_evaluator_output: serialized_output,
                                });
                                records.push(EvaluatorExecutionRecord {
                                    evaluator_id: evaluator_entry.evaluator_id,
                                    status,
                                    binding_dimension: binding.dimension,
                                    source_dimension: None,
                                    normalized_score: None,
                                    blocking: binding.blocking || dimension_policy_blocking,
                                    binding_weight: binding.weight,
                                    failure_category,
                                    reason,
                                });
                            } else {
                                for (finding_index, finding) in normalized.into_iter().enumerate() {
                                    let finding_index = i32::try_from(finding_index)?;
                                    let status = finding.status;
                                    let source_dimension =
                                        map_evaluation_dimension(&finding.dimension);
                                    let dimension = binding.dimension.clone();
                                    let blocking = binding.blocking
                                        || dimension_policy_blocking
                                        || finding.blocking;
                                    let severity = map_severity(&finding.severity).to_string();
                                    let failure_category = finding.failure_category.clone();
                                    let reason = finding.reason.clone();
                                    let normalized_score = finding.normalized_score;

                                    rows.push(evaluator_results::EvaluatorResultInsertRow {
                                        run_id,
                                        run_shard,
                                        execution_id,
                                        attempt_id,
                                        evaluator_id: evaluator_entry.evaluator_id,
                                        finding_index,
                                        evaluator_version: evaluator_entry.evaluator_version.clone(),
                                        evaluator_profile_id: profile_id.clone(),
                                        evaluator_profile_version: profile_version.clone(),
                                        evaluator_interface_version: evaluator_entry
                                            .evaluator_interface_version
                                            .clone(),
                                        evaluator_runtime_version: evaluator_entry
                                            .evaluator_runtime_version
                                            .clone(),
                                        dimension: dimension.clone(),
                                        status: map_evaluation_status(&status).to_string(),
                                        blocking,
                                        score_kind: finding.score_kind,
                                        raw_score: finding.raw_score,
                                        raw_score_min: finding.raw_score_min,
                                        raw_score_max: finding.raw_score_max,
                                        normalized_score,
                                        weight: binding.weight,
                                        severity,
                                        failure_category: failure_category.clone(),
                                        reason: reason.clone(),
                                        evidence: finding.evidence,
                                        raw_evaluator_output: serialized_output.clone(),
                                    });
                                    records.push(EvaluatorExecutionRecord {
                                        evaluator_id: evaluator_entry.evaluator_id,
                                        status,
                                        binding_dimension: dimension,
                                        source_dimension: Some(source_dimension),
                                        normalized_score,
                                        blocking,
                                        binding_weight: binding.weight,
                                        failure_category,
                                        reason,
                                    });
                                }
                            }
                        }
                        Err(err) => {
                            let status = EvaluationStatus::Error;
                            let failure_category = Some("evaluator_runtime_error".to_string());
                            let reason = Some(err.to_string());
                            rows.push(evaluator_results::EvaluatorResultInsertRow {
                                run_id,
                                run_shard,
                                execution_id,
                                attempt_id,
                                evaluator_id: evaluator_entry.evaluator_id,
                                finding_index: 0,
                                evaluator_version: evaluator_entry.evaluator_version,
                                evaluator_profile_id: profile_id,
                                evaluator_profile_version: profile_version,
                                evaluator_interface_version: evaluator_entry
                                    .evaluator_interface_version,
                                evaluator_runtime_version: evaluator_entry.evaluator_runtime_version,
                                dimension: binding.dimension.clone(),
                                status: map_evaluation_status(&status).to_string(),
                                blocking: binding.blocking || dimension_policy_blocking,
                                score_kind: "informational".to_string(),
                                raw_score: None,
                                raw_score_min: None,
                                raw_score_max: None,
                                normalized_score: None,
                                weight: binding.weight,
                                severity: "high".to_string(),
                                failure_category: failure_category.clone(),
                                reason: reason.clone(),
                                evidence: json!({}),
                                raw_evaluator_output: json!({
                                    "error": err.to_string()
                                }),
                            });
                            records.push(EvaluatorExecutionRecord {
                                evaluator_id: evaluator_entry.evaluator_id,
                                status,
                                binding_dimension: binding.dimension,
                                source_dimension: None,
                                normalized_score: None,
                                blocking: binding.blocking || dimension_policy_blocking,
                                binding_weight: binding.weight,
                                failure_category,
                                reason,
                            });
                        }
                    }

                    Ok((index, records, rows))
                });
            }

            let Some(task_result) = tasks.join_next().await else {
                continue;
            };

            let (index, record, row) = task_result
                .map_err(|err| anyhow::anyhow!("evaluator worker task join failed: {}", err))??;
            ordered_records.insert(index, record);
            ordered_rows.insert(index, row);
        }

        Ok((
            ordered_records.into_values().flatten().collect(),
            ordered_rows.into_values().flatten().collect(),
        ))
    }
    .await;

    match evaluation_result {
        Ok((records, result_rows)) => {
            let runtime_ms = runtime_started.elapsed().as_millis() as u64;
            let aggregation_findings = records
                .iter()
                .map(|record| AggregationFinding {
                    evaluator_id: record.evaluator_id,
                    binding_dimension: record.binding_dimension.clone(),
                    source_dimension: record.source_dimension.clone(),
                    status: record.status.clone(),
                    normalized_score: record.normalized_score,
                    blocking: record.blocking,
                    binding_weight: record.binding_weight,
                    failure_category: record.failure_category.clone(),
                    reason: record.reason.clone(),
                })
                .collect::<Vec<_>>();
            let aggregate = aggregate_findings(
                &run_profile.defaults,
                aggregation,
                attempt_id,
                &aggregation_findings,
            );

            let evaluator_result_count = match i32::try_from(records.len()) {
                Ok(count) => count,
                Err(err) => {
                    let failure_message = format!("case execution result count overflow: {}", err);
                    return CaseExecutionOutcome {
                        processed: Some(processed_terminal_failure(
                            execution_id,
                            attempt_id,
                            attempt_no,
                            failure_message,
                        )),
                        persistence: None,
                    };
                }
            };

            debug!(
                run_id = %run_id,
                case_id = %case.case_id,
                execution_id = %execution_id,
                attempt_id = %attempt_id,
                evaluator_results_attempted = result_rows.len(),
                runtime_ms,
                "completed case evaluator execution"
            );

            let processed = ProcessedExecution {
                execution_id,
                attempt_id,
                result_count: records.len(),
                terminal_transition: ExecutionTerminalTransition {
                    execution_id,
                    attempt_id,
                    attempt_no,
                    completed: true,
                    error_message: None,
                },
            };

            CaseExecutionOutcome {
                processed: Some(processed),
                persistence: Some(CompletedExecutionPersistence {
                    execution_id,
                    attempt_id,
                    attempt_no,
                    result_rows,
                    overall_status: aggregate.overall_status,
                    aggregate_score: aggregate.aggregate_score,
                    evaluator_result_count,
                    dimension_scores: aggregate.dimension_scores,
                    blocking_failures: aggregate.blocking_failures,
                    summary: aggregate.summary,
                }),
            }
        }
        Err(err) => {
            let error_message = err.to_string();
            let failure_message = if error_message.starts_with("agent invocation failed:") {
                error_message
            } else {
                format!("case execution processing failed: {}", error_message)
            };
            CaseExecutionOutcome {
                processed: Some(processed_terminal_failure(
                    execution_id,
                    attempt_id,
                    attempt_no,
                    failure_message,
                )),
                persistence: None,
            }
        }
    }
}

/// Persists successful evaluator evidence and execution aggregates.
///
/// Query behavior:
/// - Checks every completed record still owns the execution's current attempt.
/// - Inserts evaluator results as append-oriented evidence; uniqueness handles
///   redelivery by turning duplicate result rows into conflicts.
/// - Upserts aggregates for completed attempts after evidence insertion.
/// - Leaves execution status changes to `finalize_execution_terminal_transitions`
///   so state mutation remains one authority-checked batch.
async fn persist_completed_execution_results_batch(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    completed: &[CompletedExecutionPersistence],
) -> anyhow::Result<BatchPersistenceStats> {
    if completed.is_empty() {
        return Ok(BatchPersistenceStats::default());
    }

    // Query outline:
    //
    // authority_query - short shared run-state/current-attempt guard.
    // result insert   - append evaluator evidence for authoritative attempts.
    // aggregate upsert- summarize completed attempts for terminal transition.
    //
    // The shared run-state guard preserves cancellation/finalization ordering
    // without turning the run row into an exclusive worker-side mutex.
    let mut tx = db.begin().await?;

    let mut authority_query = QueryBuilder::<Postgres>::new(
        r#"
        WITH input (
            execution_id,
            attempt_id,
            attempt_no
        ) AS (
        "#,
    );
    authority_query.push_values(completed, |mut b, row| {
        b.push_bind(row.execution_id)
            .push_bind(row.attempt_id)
            .push_bind(row.attempt_no);
    });
    authority_query.push(
        r#"
        ),
        run_guard AS (
            SELECT id
            FROM runs
            WHERE id =
        "#,
    );
    authority_query.push_bind(run_id);
    authority_query.push(
        r#"::uuid
              AND status = 'running'::run_status
            FOR SHARE
        ),
        locked AS (
            SELECT executions.id
        FROM run_guard
        JOIN executions
          ON executions.run_id = run_guard.id
         AND executions.run_shard =
        "#,
    );
    authority_query.push_bind(run_shard);
    authority_query.push(
        r#"
        JOIN input
          ON input.execution_id = executions.id
        WHERE executions.current_attempt_id = input.attempt_id
          AND executions.current_attempt_no = input.attempt_no
        FOR UPDATE OF executions
        )
        SELECT COUNT(*)::bigint
        FROM locked
        "#,
    );

    let current_attempt_count = authority_query
        .build_query_scalar::<i64>()
        .fetch_one(&mut *tx)
        .await?;
    if usize::try_from(current_attempt_count)? != completed.len() {
        anyhow::bail!(
            "aggregate persistence batch locked {} current executions out of {}; at least one attempt lost authority",
            current_attempt_count,
            completed.len()
        );
    }

    let result_rows = completed
        .iter()
        .flat_map(|row| row.result_rows.iter().cloned())
        .collect::<Vec<_>>();
    let evaluator_results_inserted =
        evaluator_results::insert_evaluator_results_batch(&mut tx, &result_rows).await?;

    let mut aggregate_query = QueryBuilder::<Postgres>::new(
        r#"
        INSERT INTO execution_aggregates (
            execution_id,
            run_id,
            run_shard,
            attempt_id,
            overall_status,
            aggregate_score,
            evaluator_result_count,
            dimension_scores,
            blocking_failures,
            summary
        )
        "#,
    );
    aggregate_query.push_values(completed, |mut b, row| {
        b.push_bind(row.execution_id)
            .push_bind(run_id)
            .push_bind(run_shard)
            .push_bind(row.attempt_id)
            .push_bind(&row.overall_status)
            .push_unseparated("::evaluation_status")
            .push_bind(row.aggregate_score)
            .push_bind(row.evaluator_result_count)
            .push_bind(&row.dimension_scores)
            .push_bind(&row.blocking_failures)
            .push_bind(&row.summary)
            .push_unseparated("::jsonb");
    });
    aggregate_query.push(
        r#"
        ON CONFLICT (run_id, run_shard, execution_id) DO UPDATE
        SET attempt_id = EXCLUDED.attempt_id,
            overall_status = EXCLUDED.overall_status,
            aggregate_score = EXCLUDED.aggregate_score,
            evaluator_result_count = EXCLUDED.evaluator_result_count,
            dimension_scores = EXCLUDED.dimension_scores,
            blocking_failures = EXCLUDED.blocking_failures,
            summary = EXCLUDED.summary,
            updated_at = now()
        "#,
    );
    aggregate_query.build().execute(&mut *tx).await?;

    tx.commit().await?;

    let evaluator_results_attempted = result_rows.len();
    Ok(BatchPersistenceStats {
        evaluator_results_attempted,
        evaluator_results_inserted,
        evaluator_result_conflicts: evaluator_results_attempted
            .saturating_sub(evaluator_results_inserted as usize),
    })
}

/// Processes a chunk-sized batch of dataset cases.
///
/// The function allocates authoritative attempts in one statement, runs cases
/// with bounded chunk-local parallelism, runs each case's evaluators with
/// bounded per-case parallelism, and persists all completed case results in one
/// chunk-level transaction. Terminal execution transitions are returned for the
/// caller to batch-apply after persistence.
///
/// Workflow behavior:
/// - Resolve evaluator bindings from the run profile for each case.
/// - Ask the database which executions are due, waiting, terminal, or exhausted.
/// - Run only due cases; waiting and terminal cases are skipped for this pass.
/// - Convert exhausted open cases into failed terminal transitions so retry
///   loops cannot continue indefinitely.
/// - Persist successful evaluator evidence before returning terminal
///   transitions for authority-checked status updates.
pub(crate) async fn process_case_batch_execution(
    context: &Context,
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    chunk_id: Uuid,
    run_profile: &RunProfile,
    evaluator_catalog: &RunEvaluatorCatalog,
    cases: &[chunk_processing::WorkerCaseBatchItem],
) -> anyhow::Result<Vec<ProcessedExecution>> {
    if cases.is_empty() {
        return Ok(Vec::new());
    }

    let setup_started = Instant::now();
    let evaluation_plans_by_case = cases
        .iter()
        .map(|case| evaluation_plan_for_case(run_profile, case))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let allocations = allocate_execution_attempts_for_cases(
        db,
        run_id,
        run_shard,
        chunk_id,
        run_profile,
        cases,
        &evaluation_plans_by_case,
    )
    .await?;

    debug!(
        run_id = %run_id,
        case_count = cases.len(),
        setup_ms = setup_started.elapsed().as_millis() as u64,
        "initialized execution attempts for case batch"
    );

    let parallelism = CASE_EXECUTION_PARALLELISM.min(cases.len().max(1));
    let mut next_index = 0usize;
    let mut pending = FuturesUnordered::new();
    let mut ordered_outcomes = Vec::with_capacity(cases.len());

    while next_index < cases.len() || !pending.is_empty() {
        while next_index < cases.len() && pending.len() < parallelism {
            let index = next_index;
            next_index += 1;

            let case = &cases[index];
            let evaluation_plan = &evaluation_plans_by_case[index];
            let allocation = &allocations[index];
            pending.push(async move {
                if allocation.case_id != case.case_id {
                    anyhow::bail!(
                        "attempt allocation returned case '{}' while processing case '{}'",
                        allocation.case_id,
                        case.case_id
                    );
                }

                let outcome = if allocation.retry_not_due || allocation.already_terminal {
                    CaseExecutionOutcome {
                        processed: None,
                        persistence: None,
                    }
                } else if allocation.max_attempts_exhausted {
                    let attempt_id = allocation.attempt_id.ok_or_else(|| {
                        anyhow::anyhow!(
                            "case '{}' exhausted attempts without a current attempt id",
                            case.case_id
                        )
                    })?;
                    CaseExecutionOutcome {
                        processed: Some(processed_terminal_failure(
                            allocation.execution_id,
                            attempt_id,
                            allocation.attempt_no,
                            format!(
                                "execution retry budget exhausted after {} attempts",
                                allocation.attempt_no
                            ),
                        )),
                        persistence: None,
                    }
                } else if allocation.should_process {
                    evaluate_case_execution(
                        context,
                        run_id,
                        run_shard,
                        run_profile,
                        evaluator_catalog,
                        case,
                        &evaluation_plan.evaluator_bindings,
                        &evaluation_plan.aggregation,
                        allocation,
                    )
                    .await
                } else {
                    CaseExecutionOutcome {
                        processed: None,
                        persistence: None,
                    }
                };

                Ok::<_, anyhow::Error>((index, outcome))
            });
        }

        let Some(result) = pending.next().await else {
            continue;
        };
        ordered_outcomes.push(result?);
    }

    ordered_outcomes.sort_by_key(|(index, _)| *index);

    let mut processed = Vec::with_capacity(cases.len());
    let mut completed = Vec::new();
    for (_, outcome) in ordered_outcomes {
        if let Some(persistence) = outcome.persistence {
            completed.push(persistence);
        }
        if let Some(outcome) = outcome.processed {
            processed.push(outcome);
        }
    }

    let persistence_started = Instant::now();
    let persistence_stats =
        persist_completed_execution_results_batch(db, run_id, run_shard, &completed).await?;
    debug!(
        run_id = %run_id,
        case_count = cases.len(),
        case_parallelism = parallelism,
        completed_case_count = completed.len(),
        evaluator_results_attempted = persistence_stats.evaluator_results_attempted,
        evaluator_results_inserted = persistence_stats.evaluator_results_inserted,
        evaluator_result_conflicts = persistence_stats.evaluator_result_conflicts,
        persistence_ms = persistence_started.elapsed().as_millis() as u64,
        "persisted completed case results for batch"
    );

    Ok(processed)
}

/// Returns whether an evaluator lifecycle state is executable by workers.
pub(crate) fn is_runnable_evaluator_state(state: &EvaluatorState) -> bool {
    matches!(
        state,
        EvaluatorState::Active | EvaluatorState::Deprecated | EvaluatorState::Yanked
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        evaluation_plan_for_case,
        make_test_case,
        matching_groups_for_case,
    };
    use crate::{
        contracts::run::{
            AgentHttpConfig,
            AgentProfile,
            AggregationMethod,
            AggregationSettings,
            AppliesTo,
            CaseGroupProfile,
            DimensionAggregation,
            EvaluatorBinding,
            PersistRawOutputsMode,
            PersistenceMode,
            PersistenceSettings,
            RunDefaults,
            RunProfile,
        },
        db::workflows::chunk_processing::WorkerCaseBatchItem,
    };

    fn evaluator_binding(evaluator_ref: &str, dimension: &str) -> EvaluatorBinding {
        EvaluatorBinding {
            evaluator_ref: evaluator_ref.to_string(),
            dimension: dimension.to_string(),
            blocking: false,
            weight: 1.0,
            config: json!({}),
        }
    }

    fn aggregation(method: AggregationMethod) -> AggregationSettings {
        AggregationSettings {
            dimensions: [(
                "quality".to_string(),
                DimensionAggregation {
                    method,
                    blocking: false,
                    weight: 1.0,
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    fn case_group(
        id: &str,
        task_type: &str,
        tags_any: Vec<&str>,
        evaluator_ref: &str,
        method: AggregationMethod,
    ) -> CaseGroupProfile {
        CaseGroupProfile {
            id: id.to_string(),
            description: format!("{id} group"),
            applies_to: AppliesTo {
                task_type: task_type.to_string(),
                tags_any: tags_any.into_iter().map(ToOwned::to_owned).collect(),
                tags_all: vec![],
            },
            evaluators: vec![evaluator_binding(evaluator_ref, "quality")],
            aggregation: aggregation(method),
        }
    }

    fn run_profile(groups: Vec<CaseGroupProfile>) -> RunProfile {
        RunProfile {
            profile_id: "profile".to_string(),
            profile_version: "1.0.0".to_string(),
            description: "test profile".to_string(),
            defaults: RunDefaults {
                max_attempts: 1,
                request_timeout_secs: 30,
                fail_on_any_blocking_failure: true,
                min_execution_score: 0.8,
            },
            persistence: PersistenceSettings {
                mode: PersistenceMode::Full,
                persist_raw_outputs: PersistRawOutputsMode::All,
                persist_evaluator_evidence: true,
            },
            agent: AgentProfile {
                provider: "example".to_string(),
                name: "agent".to_string(),
                version: None,
                model: None,
                prompt_config_id: None,
                prompt_config_version: None,
                http: AgentHttpConfig {
                    url: "http://127.0.0.1:8787/v1/agent/invoke".to_string(),
                    method: "POST".to_string(),
                    headers: Default::default(),
                    timeout_secs: None,
                },
                config: json!({}),
            },
            case_groups: groups,
        }
    }

    fn worker_case(case_group: Option<&str>) -> WorkerCaseBatchItem {
        WorkerCaseBatchItem {
            case_id: Uuid::parse_str("018f1111-1111-7111-8111-111111111101").unwrap(),
            case_hash: "case-hash".to_string(),
            case_ordinal: 0,
            task_type: "classification".to_string(),
            case_group: case_group.map(ToOwned::to_owned),
            input_payload: json!({"user_message": "I love this product."}),
            expected_output: json!({"label": "positive"}),
            context_payload: serde_json::Value::Null,
            tags: json!(["sentiment"]),
            metadata: json!({}),
        }
    }

    fn routing_profile() -> RunProfile {
        run_profile(vec![
            case_group(
                "sentiment_classification",
                "classification",
                vec!["sentiment"],
                "vigilo/sentiment-basic-en:0.1.0",
                AggregationMethod::WeightedMean,
            ),
            case_group(
                "json_contract",
                "json_contract",
                vec![],
                "core/json-schema:1.0.0",
                AggregationMethod::MinScore,
            ),
        ])
    }

    #[test]
    fn explicit_case_group_bypasses_task_and_tag_matching() {
        let profile = routing_profile();
        let case = worker_case(Some("json_contract"));

        let groups = matching_groups_for_case(&profile, &case);
        let plan = evaluation_plan_for_case(&profile, &case).unwrap();

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, "json_contract");
        assert_eq!(plan.profile_group_id, "json_contract");
        assert_eq!(plan.evaluator_bindings.len(), 1);
        assert_eq!(
            plan.evaluator_bindings[0].evaluator_ref,
            "core/json-schema:1.0.0"
        );
    }

    #[test]
    fn omitted_case_group_uses_task_and_tag_matching() {
        let profile = routing_profile();
        let case = worker_case(None);

        let groups = matching_groups_for_case(&profile, &case);
        let plan = evaluation_plan_for_case(&profile, &case).unwrap();

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, "sentiment_classification");
        assert_eq!(plan.profile_group_id, "sentiment_classification");
        assert_eq!(
            plan.evaluator_bindings[0].evaluator_ref,
            "vigilo/sentiment-basic-en:0.1.0"
        );
    }

    #[test]
    fn unknown_explicit_case_group_does_not_fall_back_to_task_tags() {
        let profile = routing_profile();
        let case = worker_case(Some("missing_group"));

        let groups = matching_groups_for_case(&profile, &case);
        let err = evaluation_plan_for_case(&profile, &case).unwrap_err();

        assert!(groups.is_empty());
        assert!(err.to_string().contains("did not match any evaluator bindings"));
    }

    #[test]
    fn automatic_matching_rejects_conflicting_case_group_policies() {
        let profile = run_profile(vec![
            case_group(
                "quality_weighted",
                "classification",
                vec![],
                "core/quality-a:1.0.0",
                AggregationMethod::WeightedMean,
            ),
            case_group(
                "quality_min",
                "classification",
                vec![],
                "core/quality-b:1.0.0",
                AggregationMethod::MinScore,
            ),
        ]);
        let case = worker_case(None);

        let err = evaluation_plan_for_case(&profile, &case).unwrap_err();

        assert!(err.to_string().contains("conflicting aggregation policy"));
    }

    #[test]
    fn make_test_case_preserves_case_group_for_evaluator_input() {
        let case = worker_case(Some("sentiment_classification"));

        let test_case = make_test_case(&case).unwrap();

        assert_eq!(
            test_case.case_group.as_deref(),
            Some("sentiment_classification")
        );
    }
}
