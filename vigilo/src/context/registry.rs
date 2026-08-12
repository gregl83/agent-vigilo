//! Prepared evaluator registry cache context.
//!
//! Workers reuse Wasmtime components by artifact hash, immutable WIT contract
//! hash, and host adapter id. Values are weighted by approximate compiled image
//! size so the cache stays within the configured memory budget.

use moka::future::Cache;
use tokio::sync::OnceCell;
use tracing::debug;

use crate::evaluator_abi::PreparedEvaluator;

pub struct Context {
    pub(crate) cell: OnceCell<Cache<String, PreparedEvaluator>>,
}

impl Context {
    pub async fn get(&self) -> anyhow::Result<&Cache<String, PreparedEvaluator>> {
        self.cell
            .get_or_try_init(|| async {
                debug!("initializing evaluators registry");

                let cache = Cache::builder()
                    .weigher(|_key: &String, evaluator: &PreparedEvaluator| {
                        let size = evaluator.approximate_size();
                        // The weigher returns u32; cap enormous modules at u32::MAX.
                        size.try_into().unwrap_or(u32::MAX)
                    })
                    .max_capacity(512 * 1024 * 1024)
                    .build();

                Ok(cache)
            })
            .await
    }
}
