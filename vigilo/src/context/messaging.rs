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
            .get_or_try_init(|| async {
                Ok(
                    Client::new(self.config.clone())
                )
            })
            .await
    }
}




