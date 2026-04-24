use moka::future::Cache;
use tokio::sync::OnceCell;
use tracing::debug;
use wasmtime::Module;

pub struct Context {
    pub(crate) cell: OnceCell<Cache<String, Module>>,
}

impl Context {
    pub async fn get(&self) -> anyhow::Result<&Cache<String, Module>> {
        self.cell.get_or_try_init(|| async {
            debug!("initializing evaluators registry");

            let cache = Cache::builder()
                .weigher(|_key: &String, module: &Module| {
                    // approximate compiled module size in bytes
                    // module::image_range gives the mapped memory region size
                    let range = module.image_range();
                    let size = range.end as usize - range.start as usize;
                    // weigher must return u32 — cap at u32::MAX for enormous modules
                    size.try_into().unwrap_or(u32::MAX)
                })
                .max_capacity(512 * 1024 * 1024)
                .build();

            Ok(cache)
        }).await
    }
}
