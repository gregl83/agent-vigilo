use sqlx::postgres::PgPoolOptions;
use url::Url;
use uuid::Uuid;

pub(crate) async fn isolated_postgres_urls(
    primary_url: &str,
    shard_url: &str,
) -> anyhow::Result<(String, String)> {
    let schema = format!("vigilo_test_{}", Uuid::now_v7().simple());
    let primary_url = create_schema_url(primary_url, &schema).await?;
    let shard_url = create_schema_url(shard_url, &schema).await?;
    Ok((primary_url, shard_url))
}

async fn create_schema_url(database_url: &str, schema: &str) -> anyhow::Result<String> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(database_url)
        .await?;
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
        .execute(&pool)
        .await?;
    pool.close().await;

    let mut url = Url::parse(database_url)?;
    url.query_pairs_mut()
        .append_pair("options[search_path]", schema);
    Ok(url.into())
}
