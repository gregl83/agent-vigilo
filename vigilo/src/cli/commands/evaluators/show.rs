use super::*;

pub(super) async fn exec(context: Context, evaluator: String) -> anyhow::Result<()> {
    info!("fetching evaluator {}", evaluator);

    let db = context.db().await?;
    let out = context.out().await?;
    let evaluator = parse_fully_qualified_evaluator(&evaluator)?;

    let evaluator_record = evaluators::select_evaluator(
        db,
        &evaluator.namespace,
        &evaluator.name,
        &evaluator.version,
    )
    .await?;

    let payload = match evaluator_record {
        Some(e) => json!({
            "data": {
                "id": e.id,
                "namespace": e.namespace,
                "name": e.name,
                "version": e.version,
                "content_hash": e.content_hash,
                "wasm_size_bytes": e.wasm_size_bytes,
                "interface_name": e.interface_name,
                "interface_version": e.interface_version,
                "wit_world": e.wit_world,
                "runtime": e.runtime,
                "runtime_version": e.runtime_version,
                "runtime_fingerprint": e.runtime_fingerprint,
                "description": e.description,
                "tags": e.tags,
                "metadata": e.metadata,
                "state": e.state,
                "state_reason": e.state_reason,
                "created_at": e.created_at,
                "updated_at": e.updated_at,
            }
        }),
        None => {
            anyhow::bail!(
                "evaluator not found: {}/{}:{}",
                evaluator.namespace,
                evaluator.name,
                evaluator.version,
            );
        }
    };

    out.write_value(&payload)?;

    Ok(())
}
