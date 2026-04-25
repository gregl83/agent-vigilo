use std::{
    fs,
    path::PathBuf,
    hash::{DefaultHasher, Hash, Hasher},
};

use async_trait::async_trait;
use clap::{crate_name, crate_version, Args, Subcommand};
use tracing::info;
use wasmtime::{
    Config,
    component::Component,
    Engine,
};

use crate::context::Context;
use super::args::parsers::parse_filepath;
use super::Executable;


#[derive(Debug, Subcommand)]
pub(crate) enum SubCommand {
    /// Add evaluator to system
    Add {
        /// Path to evaluator
        #[arg(value_parser = parse_filepath)]
        evaluator_path: PathBuf,
    },
    /// Show system evaluator
    Show {
        /// Evaluator name
        #[arg()]
        evaluator_name: String,
    },
    /// Deactivate system evaluator
    Deactivate {
        /// Evaluator name
        #[arg()]
        evaluator_name: String,
    },
    /// Activate system evaluator
    Activate {
        /// Evaluator name
        #[arg()]
        evaluator_name: String,
    },
    /// Remove system evaluator
    Remove {
        /// Evaluator name
        #[arg()]
        evaluator_name: String,
    },
}

#[async_trait]
impl Executable for SubCommand {
    async fn exec(self, _context: Context) -> anyhow::Result<()> {
        match self {
            SubCommand::Add{ evaluator_path } => {
                println!("executing run");

                info!(
                    "adding evaluator: {}@{}:{}",
                    crate_name!(),
                    crate_version!(),
                    evaluator_path.display(),
                );

                let wasm_bytes = fs::read(evaluator_path)?;

                let mut config = Config::new();
                config.wasm_component_model(true);

                let engine = Engine::new(&config)?;

                let component = Component::new(
                    &engine,
                    &wasm_bytes
                )?;

                fn engine_fingerprint(engine: &Engine) -> String {
                    let mut hasher = DefaultHasher::new();
                    engine.precompile_compatibility_hash().hash(&mut hasher);
                    format!("{:x}-{}", hasher.finish(), std::env::consts::ARCH)
                }

                let hash = blake3::hash(&wasm_bytes).to_hex().to_string();
                let compiled = component.serialize()?;
                let fingerprint = engine_fingerprint(&engine);
                println!("{}", fingerprint);

                // todo - move wasm code to module

                // Example async work
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("run complete");
                Ok(())
            }
            SubCommand::Show{ evaluator_name } => {
                println!("executing run");

                // todo

                // Example async work
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("run complete");
                Ok(())
            }
            SubCommand::Deactivate{ evaluator_name } => {
                println!("executing run");

                // todo

                // Example async work
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("run complete");
                Ok(())
            }
            SubCommand::Activate{ evaluator_name } => {
                println!("executing run");

                // todo

                // Example async work
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("run complete");
                Ok(())
            }
            SubCommand::Remove{ evaluator_name } => {
                println!("executing run");

                // todo

                // Example async work
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("run complete");
                Ok(())
            }
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct Command {
    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

#[async_trait]
impl Executable for Command {
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self.command {
            Some(subcommand) => subcommand.exec(context).await,
            None => {

                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                println!("Command complete");
                Ok(())
            }
        }
    }
}
