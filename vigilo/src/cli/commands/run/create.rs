//! Run creation command implementation.
//!
//! Validates profile/dataset inputs, derives stable hashes and dataset version
//! identity, and writes pending run work in one transaction. This module must
//! not publish queue-visible work directly; coordinator dispatch owns worker
//! visibility after the run is durably created.

use super::*;

/// Implements `vigilo run create`.
///
/// This flow validates executability, creates case/dataset/run drafts, stores
/// durable run work in one transaction, and emits a machine-readable summary.
/// Coordinators publish chunk-ready events in bounded windows after they mark
/// the run running.
pub(super) async fn exec(
    context: Context,
    profile: Option<String>,
    profile_file: Option<PathBuf>,
    dataset: Option<String>,
    dataset_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    // --- Load command inputs ---
    // Acquire output/database handles and parse profile/dataset payloads before
    // any durable writes are attempted.
    let db = context.db().await?.control().await?;
    let out = context.out().await?;

    let parsed = load_run_inputs(profile, profile_file, dataset, dataset_file)?;
    let profile_payload = canonical_json(&parsed.profile_payload);
    let dataset_payload = canonical_json(&parsed.dataset_payload);

    // --- Validate executability ---
    // Catch empty datasets, invalid agent settings, unmatched cases, and
    // missing/non-runnable evaluators before creating run rows or chunks.
    if parsed.dataset.cases.is_empty() {
        anyhow::bail!("dataset must include at least one case");
    }

    let executability = run_profile_validation::validate_profile_executability(
        db,
        &parsed.profile,
        &parsed.dataset,
    )
    .await?;

    // --- Derive immutable identities ---
    // These hashes, ids, and chunks are persisted into the run snapshot for
    // reproducibility and later export/debugging.
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

    // --- Build run configuration snapshot ---
    // The snapshot is written before dispatch makes any chunks visible.
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

    // --- Persist pending run work ---
    // Write immutable case blobs, dataset membership, the pending run, and
    // pending chunks in one transaction. Coordinator dispatch owns worker
    // visibility; no queue-visible events are emitted here.
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
    run_create::bulk_insert_run_shard_dispatch_cursors(&mut tx, run_id, &chunks).await?;

    tx.commit().await?;

    // --- Emit create response ---
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
