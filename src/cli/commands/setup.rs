use std::path::PathBuf;

use async_trait::async_trait;
use clap::Args;

use super::Executable;


#[derive(Debug, Args)]
pub struct Command {
    /// Path to migrations directory
    #[arg(long, default_value = "migrations")]
    pub migrations_dir: PathBuf,
}

#[async_trait]
impl Executable for Command {
    async fn exec(self) -> anyhow::Result<()> {
        // todo - sqlx migration call

        println!(
            "Setting up Agent Vigilo migrations_dir={}",
            self.migrations_dir.display()
        );

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        println!("Command complete");

        Ok(())
    }
}
