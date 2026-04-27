use std::sync::Arc;

pub(crate) mod database;
pub(crate) mod output;
pub(crate) mod registry;
pub(crate) mod wasm;


struct ContextInner {
    pub db: database::Context,
    pub out: output::Context,
    pub reg: registry::Context,
    pub wasm: wasm::Context,
}

#[derive(Clone)]
pub(crate) struct Context(Arc<ContextInner>);

impl Context {
    pub fn new(db_uri: String, wasm_config: wasm::Config) -> Self {
        Self(Arc::new(ContextInner {
            db: database::Context {
                uri: db_uri,
                cell: Default::default(),
            },
            out: output::Context {
                cell: Default::default(),
            },
            reg: registry::Context{
                cell: Default::default(),
            },
            wasm: wasm::Context{
                cell: Default::default(),
                config: wasm_config,
            },
        }))
    }

    pub async fn db(&self) -> anyhow::Result<&sqlx::PgPool> {
        self.0.db.get().await
    }

    pub async fn out(&self) -> anyhow::Result<&output::Buffer> {
        self.0.out.get().await
    }

    pub async fn reg(&self) -> anyhow::Result<&moka::future::Cache<String, wasmtime::component::Component>> {
        self.0.reg.get().await
    }

    pub async fn wasm(&self) -> anyhow::Result<&wasm::Wasm> {
        self.0.wasm.get().await
    }
}
