//! Lazily initialized process context.
//!
//! Commands receive a cloned [`Context`] and request services on demand:
//! database pool, HTTP client, message queue client, output buffer, evaluator
//! registry cache, and Wasm runtime. Service modules in `context::*` should
//! remain thin initialization boundaries; domain behavior belongs in runtime,
//! db, mq, or command modules.

//! Lazily initialized process context.
//!
//! Commands receive a cloned [`Context`] and request services on demand:
//! database pool, HTTP client, message queue client, output buffer, evaluator
//! registry cache, and Wasm runtime. Service modules in `context::*` should
//! remain thin initialization boundaries; domain behavior belongs in runtime,
//! db, mq, or command modules.

use std::sync::Arc;

pub(crate) mod database;
pub(crate) mod http;
pub(crate) mod messaging;
pub(crate) mod output;
pub(crate) mod registry;
pub(crate) mod wasm;

struct ContextInner {
    pub db: database::Context,
    pub http: http::Context,
    pub mq: messaging::Context,
    pub out: output::Context,
    pub reg: registry::Context,
    pub wasm: wasm::Context,
}

#[derive(Clone)]
pub(crate) struct Context(Arc<ContextInner>);

impl Context {
    pub fn new(
        db_uri: String,
        db_max_connections: u32,
        mq_uri: String,
        wasm_config: wasm::Config,
        output_format: output::OutputFormat,
    ) -> Self {
        Self(Arc::new(ContextInner {
            db: database::Context {
                uri: db_uri,
                max_connections: db_max_connections,
                cell: Default::default(),
            },
            http: http::Context {
                cell: Default::default(),
            },
            mq: messaging::Context {
                config: crate::mq::Config::new(mq_uri),
                cell: Default::default(),
            },
            out: output::Context {
                cell: Default::default(),
                format: output_format,
            },
            reg: registry::Context {
                cell: Default::default(),
            },
            wasm: wasm::Context {
                cell: Default::default(),
                config: wasm_config,
            },
        }))
    }

    pub async fn db(&self) -> anyhow::Result<&sqlx::PgPool> {
        self.0.db.get().await
    }

    pub async fn http(&self) -> anyhow::Result<&reqwest::Client> {
        self.0.http.get().await
    }

    pub async fn out(&self) -> anyhow::Result<&output::Buffer> {
        self.0.out.get().await
    }

    pub async fn mq(&self) -> anyhow::Result<&crate::mq::Client> {
        self.0.mq.get().await
    }

    pub async fn reg(
        &self,
    ) -> anyhow::Result<&moka::future::Cache<String, wasmtime::component::Component>> {
        self.0.reg.get().await
    }

    pub async fn wasm(&self) -> anyhow::Result<&wasm::Wasm> {
        self.0.wasm.get().await
    }
}
