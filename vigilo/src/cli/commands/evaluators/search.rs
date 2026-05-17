use super::*;

pub(super) async fn exec(
    context: Context,
    namespace: String,
    limit: i64,
    query: Option<String>,
) -> anyhow::Result<()> {
    info!(
        "searching evaluators namespace `{}` for term `{}`",
        namespace,
        query.clone().unwrap_or_default(),
    );

    let db = context.db().await?;
    let out = context.out().await?;
    let evaluators =
        evaluators::search_evaluator_summaries(db, &namespace, query.as_deref(), limit).await?;

    let payload = json!({
        "data": evaluators,
        "meta": {
            "namespace": namespace,
            "query": query,
            "limit": limit,
            "count": evaluators.len(),
        },
    });

    out.write_value(&payload)?;

    Ok(())
}
