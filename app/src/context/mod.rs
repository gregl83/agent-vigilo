use std::sync::Arc;

mod database;
mod output;
mod registry;

struct ContextInner {
    pub db: database::Context,
    pub out: output::Context,
    pub reg: registry::Context,
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
            reg: registry::Context{
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

    pub async fn reg(&self) -> anyhow::Result<&moka::future::Cache<String, wasmtime::Module>> {
        self.0.reg.get().await
    }
}
