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
    context::Context,
    contracts::{
        evaluator::{
            AgentOutput,
            EvaluationStatus,
            EvaluatorInput,
            Severity,
            TestCase,
        },
        evaluator_ref::parse_fully_qualified_evaluator,
        run::{
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
    status: String,
    dimension: String,
    normalized_score: Option<f64>,
    blocking: bool,
    failure_category: Option<String>,
    reason: Option<String>,
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

const EVALUATOR_EXECUTION_PARALLELISM: usize = 8;

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
    case_id: String,
    execution_id: Uuid,
    attempt_id: Uuid,
    attempt_no: i32,
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
    processed: ProcessedExecution,
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

fn evaluator_bindings_for_case(
    profile: &RunProfile,
    case: &chunk_processing::WorkerCaseBatchItem,
) -> Vec<EvaluatorBinding> {
    let mut unique = BTreeMap::new();
    for group in matching_groups_for_case(profile, case) {
        for binding in &group.evaluators {
            unique
                .entry(binding.evaluator_ref.clone())
                .or_insert_with(|| binding.clone());
        }
    }

    unique.into_values().collect()
}

fn make_test_case(case: &chunk_processing::WorkerCaseBatchItem) -> anyhow::Result<TestCase> {
    Ok(TestCase {
        id: case.case_id.clone(),
        task_type: case.task_type.clone(),
        case_group: None,
        input: case.input_payload.clone(),
        expected: Some(case.expected_output.clone()),
        context: Some(case.context_payload.clone()),
        tags: tags_from_case_row(&case.tags),
        metadata: metadata_from_case_row(&case.metadata)?,
    })
}

fn make_agent_output(case: &chunk_processing::WorkerCaseBatchItem) -> AgentOutput {
    let fallback_text = case
        .input_payload
        .get("user_message")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);

    AgentOutput {
        text: fallback_text,
        structured: None,
        tool_calls: Vec::new(),
        trace: Vec::new(),
        raw: json!({}),
        metadata: json!({
            "source": "worker_placeholder_actual_output"
        }),
    }
}

async fn allocate_execution_attempts_for_cases(
    db: &PgPool,
    run_id: Uuid,
    run_profile: &RunProfile,
    cases: &[chunk_processing::WorkerCaseBatchItem],
    evaluator_bindings_by_case: &[Vec<EvaluatorBinding>],
) -> anyhow::Result<Vec<AttemptAllocation>> {
    if cases.is_empty() {
        return Ok(Vec::new());
    }

    if cases.len() != evaluator_bindings_by_case.len() {
        anyhow::bail!(
            "case batch has {} cases but {} evaluator binding sets",
            cases.len(),
            evaluator_bindings_by_case.len()
        );
    }

    struct AllocationInput<'a> {
        case: &'a chunk_processing::WorkerCaseBatchItem,
        evaluator_manifest: serde_json::Value,
        expected_evaluator_count: i32,
        input_ordinal: i32,
    }

    let inputs = cases
        .iter()
        .zip(evaluator_bindings_by_case)
        .enumerate()
        .map(|(index, (case, evaluator_bindings))| {
            Ok(AllocationInput {
                case,
                evaluator_manifest: serde_json::to_value(evaluator_bindings)?,
                expected_evaluator_count: i32::try_from(evaluator_bindings.len())?,
                input_ordinal: i32::try_from(index)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

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
            .push_bind(&row.evaluator_manifest)
            .push_bind(row.expected_evaluator_count)
            .push_bind(row.input_ordinal);
    });

    query_builder.push(
        r#"
        ),
        upserted AS (
            INSERT INTO executions (
                run_id,
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
                input.case_id,
                input.case_hash,
                'default',
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
            ON CONFLICT (run_id, case_id) DO UPDATE
            SET case_hash = EXCLUDED.case_hash,
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
            RETURNING id, case_id, run_id
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
            FROM upserted
            WHERE execution_attempts.run_id = upserted.run_id
              AND execution_attempts.execution_id = upserted.id
              AND execution_attempts.status = 'running'::attempt_status
            RETURNING execution_attempts.id
        ),
        bumped AS (
            UPDATE executions
            SET status = 'running'::execution_status,
                current_attempt_no = executions.current_attempt_no + 1,
                last_error_message = NULL,
                started_at = COALESCE(executions.started_at, now()),
                completed_at = NULL,
                updated_at = now()
            FROM upserted
            WHERE executions.run_id = upserted.run_id
              AND executions.id = upserted.id
            RETURNING
                executions.id AS execution_id,
                executions.run_id,
                executions.current_attempt_no AS attempt_no
        ),
        inserted_attempt AS (
            INSERT INTO execution_attempts (
                execution_id,
                run_id,
                attempt_no,
                status,
                started_at,
                created_at,
                updated_at
            )
            SELECT
                bumped.execution_id,
                bumped.run_id,
                bumped.attempt_no,
                'running'::attempt_status,
                now(),
                now(),
                now()
            FROM bumped
            RETURNING id AS attempt_id, execution_id, run_id, attempt_no
        ),
        updated_execution AS (
            UPDATE executions
            SET current_attempt_id = inserted_attempt.attempt_id
            FROM inserted_attempt
            WHERE executions.run_id = inserted_attempt.run_id
              AND executions.id = inserted_attempt.execution_id
            RETURNING executions.id
        )
        SELECT
            input.case_id,
            inserted_attempt.execution_id,
            inserted_attempt.attempt_id,
            inserted_attempt.attempt_no
        FROM inserted_attempt
        JOIN upserted
          ON upserted.id = inserted_attempt.execution_id
        JOIN input
          ON input.case_id = upserted.case_id
        ORDER BY input.input_ordinal
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
/// workers cannot mark executions terminal.
pub(crate) async fn finalize_execution_terminal_transitions(
    db: &PgPool,
    run_id: Uuid,
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
        authoritative_input AS (
            SELECT
                transition_input.*,
                executions.run_id
            FROM transition_input
            JOIN executions
              ON executions.run_id = $6::uuid
             AND executions.id = transition_input.execution_id
             AND executions.current_attempt_id = transition_input.attempt_id
             AND executions.current_attempt_no = transition_input.attempt_no
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
             AND executions.id = transition_input.execution_id
            WHERE execution_attempts.run_id = $6::uuid
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
              AND execution_attempts.id = terminal_input.attempt_id
              AND execution_attempts.execution_id = terminal_input.execution_id
            RETURNING
                terminal_input.execution_id,
                terminal_input.run_id,
                terminal_input.attempt_id,
                terminal_input.attempt_no,
                terminal_input.completed,
                terminal_input.error_message,
                terminal_input.overall_status
        ),
        failed_aggregate_upsert AS (
            INSERT INTO execution_aggregates (
                execution_id,
                run_id,
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
            ON CONFLICT (run_id, execution_id) DO UPDATE
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
                    ELSE 'failed'::execution_status
                END,
                current_attempt_no = attempt_update.attempt_no,
                current_attempt_id = attempt_update.attempt_id,
                last_error_message = CASE
                    WHEN attempt_update.completed THEN NULL
                    ELSE attempt_update.error_message
                END,
                completed_at = now(),
                updated_at = now()
            FROM attempt_update
            LEFT JOIN failed_aggregate_upsert
              ON failed_aggregate_upsert.execution_id = attempt_update.execution_id
            WHERE executions.run_id = $6::uuid
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

fn summarize_overall_status(records: &[EvaluatorExecutionRecord]) -> String {
    if records
        .iter()
        .any(|record| record.blocking && (record.status == "failed" || record.status == "error"))
    {
        return "failed".to_string();
    }

    if records.iter().any(|record| record.status == "error") {
        return "error".to_string();
    }

    if records.iter().any(|record| record.status == "failed") {
        return "failed".to_string();
    }

    "passed".to_string()
}

fn map_evaluation_status(status: &EvaluationStatus) -> &'static str {
    match status {
        EvaluationStatus::Passed => "passed",
        EvaluationStatus::Failed => "failed",
        EvaluationStatus::Error => "error",
        EvaluationStatus::Skipped => "skipped",
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
    run_profile: &RunProfile,
    evaluator_catalog: &RunEvaluatorCatalog,
    case: &chunk_processing::WorkerCaseBatchItem,
    evaluator_bindings: &[EvaluatorBinding],
    allocation: &AttemptAllocation,
) -> CaseExecutionOutcome {
    let runtime_started = Instant::now();
    let execution_id = allocation.execution_id;
    let attempt_id = allocation.attempt_id;
    let attempt_no = allocation.attempt_no;

    let evaluation_result: anyhow::Result<(
        Vec<EvaluatorExecutionRecord>,
        Vec<evaluator_results::EvaluatorResultInsertRow>,
    )> = async {
        let test_case = make_test_case(case)?;
        let agent_output = make_agent_output(case);

        let mut ordered_records = BTreeMap::new();
        let mut ordered_rows = BTreeMap::new();
        let parallelism = EVALUATOR_EXECUTION_PARALLELISM.min(evaluator_bindings.len().max(1));
        let profile_id = run_profile.profile_id.clone();
        let profile_version = run_profile.profile_version.clone();
        let mut next_index = 0usize;
        let mut tasks = JoinSet::<
            anyhow::Result<(
                usize,
                EvaluatorExecutionRecord,
                evaluator_results::EvaluatorResultInsertRow,
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
                let context = context.clone();
                let test_case = test_case.clone();
                let agent_output = agent_output.clone();
                let profile_id = profile_id.clone();
                let profile_version = profile_version.clone();
                tasks.spawn(async move {
                    let wasm = context.wasm().await?.clone();
                    let component = get_or_load_component(&context, &binding.evaluator_ref).await?;

                    let evaluator_input = EvaluatorInput {
                        run_id: run_id.to_string(),
                        execution_id: execution_id.to_string(),
                        attempt_id: attempt_id.to_string(),
                        case: test_case,
                        actual: agent_output,
                        evaluator_config: binding.config.clone(),
                    };
                    let output = task::spawn_blocking(move || {
                        wasm.test_evaluator_component(&component, evaluator_input)
                    })
                    .await
                    .map_err(|err| {
                        anyhow::anyhow!("evaluator blocking task join failed: {}", err)
                    })?;

                    let (
                        status,
                        score_kind,
                        raw_score,
                        raw_score_min,
                        raw_score_max,
                        normalized_score,
                        severity,
                        failure_category,
                        reason,
                        evidence,
                        raw_evaluator_output,
                    ) = match output {
                        Ok(evaluator_output) => {
                            let serialized_output = serde_json::to_value(&evaluator_output)?;
                            let normalized = evaluator_output.normalize();

                            if let Some(primary) = normalized.first() {
                                let mut reason = primary.reason.clone();
                                if normalized.len() > 1 {
                                    let suffix = format!(
                                        " (worker persisted only the first finding out of {})",
                                        normalized.len()
                                    );
                                    reason = Some(match reason {
                                        Some(existing) => format!("{}{}", existing, suffix),
                                        None => {
                                            format!(
                                                "multiple evaluator findings returned{}",
                                                suffix
                                            )
                                        }
                                    });
                                }

                                (
                                    map_evaluation_status(&primary.status).to_string(),
                                    primary.score_kind.clone(),
                                    primary.raw_score,
                                    primary.raw_score_min,
                                    primary.raw_score_max,
                                    primary.normalized_score,
                                    map_severity(&primary.severity).to_string(),
                                    primary.failure_category.clone(),
                                    reason,
                                    primary.evidence.clone(),
                                    serialized_output,
                                )
                            } else {
                                (
                                    "error".to_string(),
                                    "informational".to_string(),
                                    None,
                                    None,
                                    None,
                                    None,
                                    "high".to_string(),
                                    Some("empty_output".to_string()),
                                    Some("evaluator returned no findings".to_string()),
                                    json!({}),
                                    serialized_output,
                                )
                            }
                        }
                        Err(err) => (
                            "error".to_string(),
                            "informational".to_string(),
                            None,
                            None,
                            None,
                            None,
                            "high".to_string(),
                            Some("evaluator_runtime_error".to_string()),
                            Some(err.to_string()),
                            json!({}),
                            json!({
                                "error": err.to_string()
                            }),
                        ),
                    };

                    let row = evaluator_results::EvaluatorResultInsertRow {
                        run_id,
                        execution_id,
                        attempt_id,
                        evaluator_id: evaluator_entry.evaluator_id,
                        evaluator_version: evaluator_entry.evaluator_version,
                        evaluator_profile_id: profile_id,
                        evaluator_profile_version: profile_version,
                        evaluator_interface_version: evaluator_entry.evaluator_interface_version,
                        evaluator_runtime_version: evaluator_entry.evaluator_runtime_version,
                        dimension: binding.dimension.clone(),
                        status: status.clone(),
                        blocking: binding.blocking,
                        score_kind,
                        raw_score,
                        raw_score_min,
                        raw_score_max,
                        normalized_score,
                        weight: binding.weight,
                        severity,
                        failure_category: failure_category.clone(),
                        reason: reason.clone(),
                        evidence,
                        raw_evaluator_output,
                    };

                    let record = EvaluatorExecutionRecord {
                        evaluator_id: evaluator_entry.evaluator_id,
                        status,
                        dimension: binding.dimension,
                        normalized_score,
                        blocking: binding.blocking,
                        failure_category,
                        reason,
                    };

                    Ok((index, record, row))
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
            ordered_records.into_values().collect(),
            ordered_rows.into_values().collect(),
        ))
    }
    .await;

    match evaluation_result {
        Ok((records, result_rows)) => {
            let runtime_ms = runtime_started.elapsed().as_millis() as u64;
            let overall_status = summarize_overall_status(&records);

            let mut by_dimension: BTreeMap<String, (f64, usize)> = BTreeMap::new();
            for record in &records {
                if let Some(score) = record.normalized_score {
                    let entry = by_dimension
                        .entry(record.dimension.clone())
                        .or_insert((0.0, 0));
                    entry.0 += score;
                    entry.1 += 1;
                }
            }

            let mut dimension_scores = serde_json::Map::new();
            for (dimension, (score_sum, score_count)) in by_dimension {
                if score_count > 0 {
                    dimension_scores.insert(dimension, json!(score_sum / score_count as f64));
                }
            }

            let blocking_failures = records
                .iter()
                .filter(|record| {
                    record.blocking && (record.status == "failed" || record.status == "error")
                })
                .map(|record| {
                    json!({
                        "evaluator_id": record.evaluator_id,
                        "dimension": record.dimension,
                        "status": record.status,
                        "failure_category": record.failure_category,
                        "reason": record.reason,
                    })
                })
                .collect::<Vec<_>>();

            let aggregate_score = {
                let scores: Vec<f64> = records
                    .iter()
                    .filter_map(|record| record.normalized_score)
                    .collect();
                if scores.is_empty() {
                    None
                } else {
                    Some(scores.iter().sum::<f64>() / scores.len() as f64)
                }
            };

            let evaluator_result_count = match i32::try_from(records.len()) {
                Ok(count) => count,
                Err(err) => {
                    let failure_message = format!("case execution result count overflow: {}", err);
                    return CaseExecutionOutcome {
                        processed: processed_terminal_failure(
                            execution_id,
                            attempt_id,
                            attempt_no,
                            failure_message,
                        ),
                        persistence: None,
                    };
                }
            };

            let summary = json!({
                "attempt_id": attempt_id,
                "result_count": records.len(),
                "overall_status": overall_status,
            });

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
                processed,
                persistence: Some(CompletedExecutionPersistence {
                    execution_id,
                    attempt_id,
                    attempt_no,
                    result_rows,
                    overall_status,
                    aggregate_score,
                    evaluator_result_count,
                    dimension_scores: serde_json::Value::Object(dimension_scores),
                    blocking_failures: serde_json::Value::Array(blocking_failures),
                    summary,
                }),
            }
        }
        Err(err) => {
            let failure_message = format!("case execution processing failed: {}", err);
            CaseExecutionOutcome {
                processed: processed_terminal_failure(
                    execution_id,
                    attempt_id,
                    attempt_no,
                    failure_message,
                ),
                persistence: None,
            }
        }
    }
}

async fn persist_completed_execution_results_batch(
    db: &PgPool,
    run_id: Uuid,
    completed: &[CompletedExecutionPersistence],
) -> anyhow::Result<BatchPersistenceStats> {
    if completed.is_empty() {
        return Ok(BatchPersistenceStats::default());
    }

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
        locked AS (
            SELECT executions.id
            FROM executions
            JOIN input
              ON executions.run_id =
        "#,
    );
    authority_query.push_bind(run_id);
    authority_query.push(
        r#"::uuid
             AND input.execution_id = executions.id
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
        ON CONFLICT (run_id, execution_id) DO UPDATE
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
/// The function allocates authoritative attempts in one statement, runs each
/// case's evaluators with bounded per-case parallelism, and persists all
/// completed case results in one chunk-level transaction. Terminal execution
/// transitions are returned for the caller to batch-apply after persistence.
pub(crate) async fn process_case_batch_execution(
    context: &Context,
    db: &PgPool,
    run_id: Uuid,
    run_profile: &RunProfile,
    evaluator_catalog: &RunEvaluatorCatalog,
    cases: &[chunk_processing::WorkerCaseBatchItem],
) -> anyhow::Result<Vec<ProcessedExecution>> {
    if cases.is_empty() {
        return Ok(Vec::new());
    }

    let setup_started = Instant::now();
    let evaluator_bindings_by_case = cases
        .iter()
        .map(|case| {
            let evaluator_bindings = evaluator_bindings_for_case(run_profile, case);
            if evaluator_bindings.is_empty() {
                anyhow::bail!(
                    "case '{}' did not match any evaluator bindings in run profile",
                    case.case_id
                );
            }
            Ok(evaluator_bindings)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let allocations = allocate_execution_attempts_for_cases(
        db,
        run_id,
        run_profile,
        cases,
        &evaluator_bindings_by_case,
    )
    .await?;

    debug!(
        run_id = %run_id,
        case_count = cases.len(),
        setup_ms = setup_started.elapsed().as_millis() as u64,
        "initialized execution attempts for case batch"
    );

    let mut processed = Vec::with_capacity(cases.len());
    let mut completed = Vec::new();
    for ((case, evaluator_bindings), allocation) in cases
        .iter()
        .zip(evaluator_bindings_by_case.iter())
        .zip(allocations.iter())
    {
        if allocation.case_id != case.case_id {
            anyhow::bail!(
                "attempt allocation returned case '{}' while processing case '{}'",
                allocation.case_id,
                case.case_id
            );
        }

        let outcome = evaluate_case_execution(
            context,
            run_id,
            run_profile,
            evaluator_catalog,
            case,
            evaluator_bindings,
            allocation,
        )
        .await;

        if let Some(persistence) = outcome.persistence {
            completed.push(persistence);
        }
        processed.push(outcome.processed);
    }

    let persistence_started = Instant::now();
    let persistence_stats =
        persist_completed_execution_results_batch(db, run_id, &completed).await?;
    debug!(
        run_id = %run_id,
        case_count = cases.len(),
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
