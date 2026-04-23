use sqlx::PgPool;
use tokio::sync::OnceCell;


pub struct Context {
    pub(crate) url: String,
    pub(crate) cell: OnceCell<PgPool>,
}

impl Context {
    pub async fn get(&self) -> anyhow::Result<&PgPool> {
        self.cell.get_or_try_init(|| async {
            // todo - configure pg pool

            PgPool::connect(&self.url).await.map_err(Into::into)
        }).await
    }
}
