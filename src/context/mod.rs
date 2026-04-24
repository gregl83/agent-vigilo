use std::sync::Arc;

mod database;
mod output;

struct ContextInner {
    pub db: database::Context,
    pub out: output::Context,
}

#[derive(Clone)]
pub(crate) struct Context(Arc<ContextInner>);

impl Context {
    pub fn new(db_uri: String) -> Self {
        Self(Arc::new(ContextInner {
            db: database::Context {
                uri: db_uri,
                cell: Default::default(),
            },
            out: output::Context {
                cell: Default::default(),
            }
        }))
    }

    pub async fn db(&self) -> anyhow::Result<&sqlx::PgPool> {
        self.0.db.get().await
    }

    pub async fn out(&self) -> anyhow::Result<&output::Buffer> {
        self.0.out.get().await
    }
}
