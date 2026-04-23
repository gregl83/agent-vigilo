use std::path::PathBuf;

use async_trait::async_trait;
use clap::Args;
use tracing::{
    info
};

use super::Executable;


// Helper function for validation
fn parse_dir(s: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(s);
    if p.is_dir() {
        Ok(p)
    } else {
        Err(format!("'{}' is not a valid directory", s))
    }
}

#[derive(Debug, Args)]
pub struct Command {
    /// Path to migrations source directory
    #[arg(long, default_value = "migrations", value_parser = parse_dir)]
    pub migrations_dir: PathBuf,
}

#[async_trait]
impl Executable for Command {
    async fn exec(self) -> anyhow::Result<()> {
        // todo - sqlx migration call

        info!(
            "Setting up Agent Vigilo migrations_dir={}",
            self.migrations_dir.display()
        );

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        info!("Command complete");

        Ok(())
    }
}
