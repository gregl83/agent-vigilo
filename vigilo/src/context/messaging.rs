//! Lazy message-queue client context.
//!
//! Coordinator and worker commands request this context when they need broker
//! access. Topology declaration, publish, consume, retry, and quarantine
//! behavior belongs in `mq`; this module only owns process-scoped lazy
//! construction from configuration.

use tokio::sync::OnceCell;

use crate::mq::{
    Client,
    Config,
};

pub struct Context {
    pub(crate) config: Config,
    pub(crate) cell: OnceCell<Client>,
}
impl Context {
    pub async fn get(&self) -> anyhow::Result<&Client> {
        self.cell
            .get_or_try_init(|| async { Ok(Client::new(self.config.clone())) })
            .await
    }
}
