use tokio::sync::OnceCell;
use tracing::debug;

pub struct Context {
    pub(crate) cell: OnceCell<reqwest::Client>,
}

impl Context {
    pub async fn get(&self) -> anyhow::Result<&reqwest::Client> {
        self.cell
            .get_or_try_init(|| async {
                debug!("initializing shared http client");

                reqwest::Client::builder()
                    .build()
                    .map_err(|e| anyhow::anyhow!("http client initialization failed: {}", e))
            })
            .await
    }
}
