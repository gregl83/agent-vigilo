use super::*;

pub(super) async fn exec(
    context: Context,
    evaluator_path: PathBuf,
    release: bool,
    profile: Option<String>,
) -> anyhow::Result<()> {
    info!("publishing evaluator: {}", evaluator_path.display());

    let profile = get_manifest_profile(release, profile);
    let component = context
        .wasm()
        .await?
        .prepare_evaluator(evaluator_path, profile)?;

    let db = context.db().await?;
    let evaluator = evaluators::insert_evaluator(
        db,
        &EvaluatorDraft {
            namespace: DEFAULT_NAMESPACE.to_string(),
            name: component.name,
            version: component.version,
            content_hash: component.wasm_hash,
            wasm_bytes: component.wasm_bytes,
            interface_name: component.interface_name,
            interface_version: component.interface_version,
            wit_world: component.wit_world,
            runtime: component.runtime,
            runtime_version: component.runtime_version,
            runtime_fingerprint: component.runtime_fingerprint,
            description: component.description,
            tags: component.tags,
            metadata: component.metadata,
        },
    )
    .await?;

    info!(
        "successfully published evaluator: {}/{}:{}",
        evaluator.namespace, evaluator.name, evaluator.version,
    );

    Ok(())
}
