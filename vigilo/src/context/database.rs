use sqlx::{
    PgPool,
    postgres::PgPoolOptions,
};
use tokio::sync::OnceCell;
use tracing::debug;

pub struct Context {
    pub(crate) uri: String,
    pub(crate) max_connections: u32,
    pub(crate) cell: OnceCell<PgPool>,
}

impl Context {
    pub async fn get(&self) -> anyhow::Result<&PgPool> {
        self.cell
            .get_or_try_init(|| async {
                debug!("initializing postgres database connection");

                PgPoolOptions::new()
                    .max_connections(self.max_connections)
                    .connect(&self.uri)
                    .await
                    .map_err(|e| anyhow::anyhow!("database connection failed: {}", e))
            })
            .await
    }
}
