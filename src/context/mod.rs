use std::sync::Arc;

mod database;

struct ContextInner {
    pub db: database::Context,
}

#[derive(Clone)]
pub(crate) struct Context(Arc<ContextInner>);

impl Context {
    pub fn new(db_url: String) -> Self {
        Self(Arc::new(ContextInner {
            db: database::Context {
                url: db_url,
                cell: Default::default(),
            },
        }))
    }

    pub async fn db(&self) -> anyhow::Result<&sqlx::PgPool> {
        self.0.db.get().await
    }
}
