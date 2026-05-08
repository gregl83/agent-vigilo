use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    time::Instant,
};

use serde_json::json;
use sqlx::PgPool;
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

#[derive(Debug, Clone)]
pub(crate) struct RunEvaluatorCatalogEntry {
    pub(crate) evaluator_id: Uuid,
    pub(crate) evaluator_version: String,
    pub(crate) evaluator_interface_version: Option<String>,
    pub(crate) evaluator_runtime_version: Option<String>,
}

pub(crate) type RunEvaluatorCatalog = BTreeMap<String, RunEvaluatorCatalogEntry>;

#[derive(Debug)]
pub(crate) struct ProcessedExecution {
    pub(crate) execution_id: Uuid,
    pub(crate) attempt_id: Uuid,
    pub(crate) result_count: usize,
}

pub(crate) fn evaluator_refs_from_snapshot(
    snapshot: &serde_json::Value,
) -> anyhow::Result<Vec<String>> {
    let profile = run_profile_from_snapshot(snapshot)?;
    evaluator_refs_from_profile(&profile)
}

pub(crate) fn evaluator_refs_from_profile(profile: &RunProfile) -> anyhow::Result<Vec<String>> {
    let mut unique_refs = BTreeSet::new();
    for group in &profile.case_groups {
        for binding in &group.evaluators {
            unique_refs.insert(binding.evaluator_ref.clone());
        }
    }

    Ok(unique_refs.into_iter().collect())
}

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

pub(crate) fn run_profile_from_snapshot(
    snapshot: &serde_json::Value,
) -> anyhow::Result<RunProfile> {
    let profile_value = snapshot
        .get("profile")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("run snapshot is missing 'profile' payload"))?;

    let profile: RunProfile = serde_json::from_value(profile_value)
        .map_err(|err| anyhow::anyhow!("run snapshot profile is invalid: {}", err))?;

    Ok(profile)
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

async fn upsert_execution_for_case(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
    run_profile: &RunProfile,
    case: &chunk_processing::WorkerCaseBatchItem,
    evaluator_bindings: &[EvaluatorBinding],
) -> anyhow::Result<(Uuid, i32)> {
    #[derive(sqlx::FromRow)]
    struct ExecutionIdentity {
        id: Uuid,
        current_attempt_no: i32,
    }

    let evaluator_manifest = serde_json::to_value(evaluator_bindings)?;

    let row = sqlx::query_as::<_, ExecutionIdentity>(
        r#"
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
        VALUES (
            $1::uuid,
            $2,
            $3,
            'default',
            $4,
            $5::jsonb,
            $6::jsonb,
            $7::jsonb,
            $8::jsonb,
            $9,
            $10,
            $11::jsonb,
            $12,
            'pending'::execution_status,
            NULL,
            now()
        )
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
        RETURNING id, current_attempt_no
        "#,
    )
    .bind(run_id)
    .bind(&case.case_id)
    .bind(&case.case_hash)
    .bind(&case.task_type)
    .bind(&case.tags)
    .bind(&case.input_payload)
    .bind(&case.expected_output)
    .bind(&case.metadata)
    .bind(&run_profile.profile_id)
    .bind(&run_profile.profile_version)
    .bind(&evaluator_manifest)
    .bind(i32::try_from(evaluator_bindings.len())?)
    .fetch_one(&mut **tx)
    .await?;

    Ok((row.id, row.current_attempt_no))
}

async fn mark_attempt_failed(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    execution_id: Uuid,
    attempt_id: Uuid,
    attempt_no: i32,
    error_message: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        UPDATE execution_attempts
        SET status = 'failed_evaluation'::attempt_status,
            error_message = $2,
            completed_at = now(),
            updated_at = now()
        WHERE id = $1::uuid
        "#,
    )
    .bind(attempt_id)
    .bind(error_message)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        r#"
        UPDATE executions
        SET status = 'failed'::execution_status,
            current_attempt_no = $2,
            current_attempt_id = $3::uuid,
            last_error_message = $4,
            completed_at = now(),
            updated_at = now()
        WHERE id = $1::uuid
        "#,
    )
    .bind(execution_id)
    .bind(attempt_no)
    .bind(attempt_id)
    .bind(error_message)
    .execute(&mut **tx)
    .await?;

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

pub(crate) async fn process_case_execution(
    context: &Context,
    db: &PgPool,
    run_id: Uuid,
    run_profile: &RunProfile,
    evaluator_catalog: &RunEvaluatorCatalog,
    case: &chunk_processing::WorkerCaseBatchItem,
) -> anyhow::Result<ProcessedExecution> {
    let evaluator_bindings = evaluator_bindings_for_case(run_profile, case);
    if evaluator_bindings.is_empty() {
        anyhow::bail!(
            "case '{}' did not match any evaluator bindings in run profile",
            case.case_id
        );
    }

    let setup_started = Instant::now();

    let (execution_id, attempt_id, attempt_no) = {
        let mut tx = db.begin().await?;

        let (execution_id, current_attempt_no) =
            upsert_execution_for_case(&mut tx, run_id, run_profile, case, &evaluator_bindings)
                .await?;
        let attempt_no = current_attempt_no + 1;

        #[derive(sqlx::FromRow)]
        struct AttemptIdentity {
            id: Uuid,
        }

        let attempt = sqlx::query_as::<_, AttemptIdentity>(
            r#"
            INSERT INTO execution_attempts (execution_id, run_id, attempt_no, status, created_at, updated_at)
            VALUES ($1::uuid, $2::uuid, $3, 'running'::attempt_status, now(), now())
            RETURNING id
            "#,
        )
        .bind(execution_id)
        .bind(run_id)
        .bind(attempt_no)
        .fetch_one(&mut *tx)
        .await?;

        let attempt_id = attempt.id;

        sqlx::query(
            r#"
            UPDATE executions
            SET status = 'running'::execution_status,
                current_attempt_no = $2,
                current_attempt_id = $3::uuid,
                last_error_message = NULL,
                started_at = COALESCE(started_at, now()),
                updated_at = now()
            WHERE id = $1::uuid
            "#,
        )
        .bind(execution_id)
        .bind(attempt_no)
        .bind(attempt_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        (execution_id, attempt.id, attempt_no)
    };

    debug!(
        run_id = %run_id,
        case_id = %case.case_id,
        execution_id = %execution_id,
        attempt_id = %attempt_id,
        setup_ms = setup_started.elapsed().as_millis() as u64,
        "initialized execution attempt"
    );

    let runtime_started = Instant::now();

    let evaluation_result: anyhow::Result<(
        Vec<EvaluatorExecutionRecord>,
        Vec<evaluator_results::EvaluatorResultInsertRow>,
    )> = async {
        let test_case = make_test_case(case)?;
        let agent_output = make_agent_output(case);
        let wasm = context.wasm().await?;

        let mut records = Vec::with_capacity(evaluator_bindings.len());
        let mut result_rows = Vec::with_capacity(evaluator_bindings.len());

        for binding in &evaluator_bindings {
            let evaluator_entry =
                evaluator_catalog
                    .get(&binding.evaluator_ref)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "evaluator '{}' is missing from run evaluator catalog",
                            binding.evaluator_ref
                        )
                    })?;

            let component = get_or_load_component(context, &binding.evaluator_ref).await?;

            let output = wasm.test_evaluator_component(
                &component,
                EvaluatorInput {
                    run_id: run_id.to_string(),
                    execution_id: execution_id.to_string(),
                    attempt_id: attempt_id.to_string(),
                    case: test_case.clone(),
                    actual: agent_output.clone(),
                    evaluator_config: binding.config.clone(),
                },
            );

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
                                None => format!("multiple evaluator findings returned{}", suffix),
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

            result_rows.push(evaluator_results::EvaluatorResultInsertRow {
                run_id,
                execution_id,
                attempt_id,
                evaluator_id: evaluator_entry.evaluator_id,
                evaluator_version: evaluator_entry.evaluator_version.clone(),
                evaluator_profile_id: run_profile.profile_id.clone(),
                evaluator_profile_version: run_profile.profile_version.clone(),
                evaluator_interface_version: evaluator_entry.evaluator_interface_version.clone(),
                evaluator_runtime_version: evaluator_entry.evaluator_runtime_version.clone(),
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
            });

            records.push(EvaluatorExecutionRecord {
                evaluator_id: evaluator_entry.evaluator_id,
                status,
                dimension: binding.dimension.clone(),
                normalized_score,
                blocking: binding.blocking,
                failure_category,
                reason,
            });
        }

        Ok((records, result_rows))
    }
    .await;

    match evaluation_result {
        Ok((records, result_rows)) => {
            let runtime_ms = runtime_started.elapsed().as_millis() as u64;
            let persistence_started = Instant::now();

            let mut tx = db.begin().await?;

            let inserted_count =
                evaluator_results::insert_evaluator_results_batch(&mut tx, &result_rows).await?;
            let replay_conflicts = result_rows.len().saturating_sub(inserted_count as usize);

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

            sqlx::query(
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
                summary,
                updated_at
            )
            VALUES (
                $1::uuid,
                $2::uuid,
                $3::uuid,
                $4::evaluation_status,
                $5,
                $6,
                $7::jsonb,
                $8::jsonb,
                $9::jsonb,
                now()
            )
            ON CONFLICT (execution_id) DO UPDATE
            SET attempt_id = EXCLUDED.attempt_id,
                overall_status = EXCLUDED.overall_status,
                aggregate_score = EXCLUDED.aggregate_score,
                evaluator_result_count = EXCLUDED.evaluator_result_count,
                dimension_scores = EXCLUDED.dimension_scores,
                blocking_failures = EXCLUDED.blocking_failures,
                summary = EXCLUDED.summary,
                updated_at = now()
            "#,
            )
            .bind(execution_id)
            .bind(run_id)
            .bind(attempt_id)
            .bind(&overall_status)
            .bind(aggregate_score)
            .bind(i32::try_from(records.len())?)
            .bind(serde_json::Value::Object(dimension_scores))
            .bind(serde_json::Value::Array(blocking_failures))
            .bind(json!({
                "attempt_id": attempt_id,
                "result_count": records.len(),
                "overall_status": overall_status,
            }))
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                r#"
            UPDATE execution_attempts
            SET status = 'completed'::attempt_status,
                error_message = NULL,
                completed_at = now(),
                updated_at = now()
            WHERE id = $1::uuid
            "#,
            )
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                r#"
            UPDATE executions
            SET status = 'completed'::execution_status,
                current_attempt_no = $2,
                current_attempt_id = $3::uuid,
                last_error_message = NULL,
                completed_at = now(),
                updated_at = now()
            WHERE id = $1::uuid
            "#,
            )
            .bind(execution_id)
            .bind(attempt_no)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?;

            tx.commit().await?;

            debug!(
                run_id = %run_id,
                case_id = %case.case_id,
                execution_id = %execution_id,
                attempt_id = %attempt_id,
                evaluator_results_attempted = result_rows.len(),
                evaluator_results_inserted = inserted_count,
                evaluator_result_conflicts = replay_conflicts,
                runtime_ms,
                persistence_ms = persistence_started.elapsed().as_millis() as u64,
                "completed case execution persistence"
            );

            Ok(ProcessedExecution {
                execution_id,
                attempt_id,
                result_count: records.len(),
            })
        }
        Err(err) => {
            let failure_message = format!("case execution processing failed: {}", err);
            let mut tx = db.begin().await?;
            mark_attempt_failed(
                &mut tx,
                execution_id,
                attempt_id,
                attempt_no,
                &failure_message,
            )
            .await?;
            tx.commit().await?;
            Err(anyhow::anyhow!(failure_message))
        }
    }
}

pub(crate) fn is_runnable_evaluator_state(state: &EvaluatorState) -> bool {
    matches!(
        state,
        EvaluatorState::Active | EvaluatorState::Deprecated | EvaluatorState::Yanked
    )
}
