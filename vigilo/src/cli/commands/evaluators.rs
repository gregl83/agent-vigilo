use std::path::PathBuf;

use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use tracing::info;

use crate::context::Context;
use crate::db::evaluators;
use crate::models::evaluator::NewEvaluator;
use super::args::parsers::parse_dir;
use super::Executable;

const DEFAULT_NAMESPACE: &str = "default";


fn get_manifest_profile(release: bool, profile: Option<String>) -> String {
    match release {
        true => "release".to_string(),
        false => profile.unwrap_or_else(|| "dev".to_string())
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum SubCommand {
    /// Add evaluator to system
    Add {
        /// Path to evaluator crate
        #[arg(value_parser = parse_dir)]
        evaluator_path: PathBuf,

        /// Add evaluator built in release mode, with optimizations
        #[arg(short, long)]
        release: bool,

        /// Add evaluator built with the specified profile
        #[arg(long, value_name = "PROFILE", conflicts_with = "release")]
        profile: Option<String>,
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
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self {
            SubCommand::Add{ evaluator_path, release, profile } => {
                info!("adding evaluator: {}", evaluator_path.display());

                let profile = get_manifest_profile(release, profile);
                let component = context.wasm().await?.build(
                    evaluator_path,
                    profile,
                )?;

                let db = context.db().await?;
                let evaluator = evaluators::insert_evaluator(
                    db,
                    &NewEvaluator {
                        namespace: DEFAULT_NAMESPACE.to_string(),
                        name: component.name,
                        version: component.version,
                        content_hash: component.wasm_hash,
                        wasm_bytes: component.wasm_bytes,
                        interface_name: None,
                        interface_version: None,
                        wit_world: None,
                        runtime: Some("wasmtime".to_string()),
                        runtime_version: None,
                        description: None,
                    },
                ).await?;

                println!(
                    "added evaluator {}:{}:{}",
                    evaluator.namespace,
                    evaluator.name,
                    evaluator.version,
                );
                Ok(())
            }
            SubCommand::Show{ evaluator_name } => {
                let db = context.db().await?;

                let evaluator = evaluators::select_latest_evaluator_by_name(
                    db,
                    DEFAULT_NAMESPACE,
                    &evaluator_name,
                ).await?;

                match evaluator {
                    Some(e) => {
                        println!(
                            "{}:{}:{} active={} hash={}",
                            e.namespace,
                            e.name,
                            e.version,
                            e.is_active,
                            e.content_hash,
                        );
                    }
                    None => {
                        println!("evaluator not found: {}", evaluator_name);
                    }
                }

                Ok(())
            }
            SubCommand::Deactivate{ evaluator_name } => {
                let db = context.db().await?;

                let affected = evaluators::update_evaluator_active_by_name(
                    db,
                    DEFAULT_NAMESPACE,
                    &evaluator_name,
                    false,
                ).await?;

                println!("deactivated {} row(s)", affected);
                Ok(())
            }
            SubCommand::Activate{ evaluator_name } => {
                let db = context.db().await?;

                let affected = evaluators::update_evaluator_active_by_name(
                    db,
                    DEFAULT_NAMESPACE,
                    &evaluator_name,
                    true,
                ).await?;

                println!("activated {} row(s)", affected);
                Ok(())
            }
            SubCommand::Remove{ evaluator_name } => {
                let db = context.db().await?;

                let affected = evaluators::delete_evaluator_by_name(
                    db,
                    DEFAULT_NAMESPACE,
                    &evaluator_name,
                ).await?;

                println!("removed {} row(s)", affected);
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
                let db = context.db().await?;
                let evaluators = evaluators::list_evaluators(db, DEFAULT_NAMESPACE).await?;

                if evaluators.is_empty() {
                    println!("no evaluators found");
                } else {
                    for evaluator in evaluators {
                        println!(
                            "{}:{}:{} active={}",
                            evaluator.namespace,
                            evaluator.name,
                            evaluator.version,
                            evaluator.is_active,
                        );
                    }
                }

                Ok(())
            }
        }
    }
}
