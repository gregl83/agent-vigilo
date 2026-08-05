//! Lazy message-queue client context.
//!
//! Coordinator and worker commands request this context when they need broker
//! access. Topology declaration, publish, consume, retry, and quarantine
//! behavior, including broker circuit admission, belongs in `mq`; this module
//! only owns process-scoped lazy construction from validated configuration.

use tokio::sync::OnceCell;

use crate::mq::{
    Client,
    Config,
};

pub struct Context {
    pub(crate) config: Option<Config>,
    pub(crate) cell: OnceCell<Client>,
}
impl Context {
    pub async fn get(&self) -> anyhow::Result<&Client> {
        let config = self.config.clone().ok_or_else(|| {
            anyhow::anyhow!("messaging configuration is unavailable for this command")
        })?;
        self.cell
            .get_or_try_init(|| async { Ok(Client::new(config)) })
            .await
    }
}
