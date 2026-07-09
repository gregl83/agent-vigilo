//! Evaluator registry commands.
//!
//! This module provides publish, search, inspect, test, and state-management
//! operations for evaluator artifacts stored in the registry.

use std::{
    fs,
    path::PathBuf,
};

use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use serde_json::json;
use tracing::{
    info,
    warn,
};

use super::{
    Executable,
    args::parsers::{
        parse_dir,
        parse_filepath,
    },
};
use crate::{
    context::Context,
    contracts::{
        evaluator::EvaluatorInput,
        evaluator_ref::parse_fully_qualified_evaluator,
    },
    db::tables::evaluators,
    models::evaluator::{
        EvaluatorPatch,
        EvaluatorState,
    },
};

mod publish;
mod search;
mod set_state;
mod show;
mod test;

const DEFAULT_NAMESPACE: &str = "vigilo";
const DEFAULT_SEARCH_LIMIT: i64 = 10;
const MAX_SEARCH_LIMIT: i64 = 20;

/// Resolves the build profile used when preparing an evaluator package.
///
/// `--release` always maps to `release`; otherwise an explicit profile is used
/// with `dev` fallback.
fn get_manifest_profile(release: bool, profile: Option<String>) -> String {
    match release {
        true => "release".to_string(),
        false => profile.unwrap_or_else(|| "dev".to_string()),
    }
}

#[derive(Debug, Subcommand)]
/// Evaluator command operations.
pub(crate) enum SubCommand {
    /// Publish evaluator version
    Publish {
        /// Path to evaluator crate
        #[arg(value_parser = parse_dir)]
        evaluator_path: PathBuf,

        /// Publish evaluator built in release mode, with optimizations
        #[arg(short, long)]
        release: bool,

        /// Publish evaluator built with the specified profile
        #[arg(long, value_name = "PROFILE", conflicts_with = "release")]
        profile: Option<String>,
    },
    /// Show evaluator details
    Show {
        /// Fully qualified evaluator identifier (<namespace>/<name>:<version>)
        #[arg()]
        evaluator: String,
    },
    /// Search evaluators
    Search {
        /// Evaluator namespace
        #[arg(long, value_name = "NAMESPACE", default_value = DEFAULT_NAMESPACE)]
        namespace: String,

        /// Max results to return
        #[arg(long, value_name = "LIMIT", default_value_t = DEFAULT_SEARCH_LIMIT, value_parser = clap::value_parser!(i64).range(1..=MAX_SEARCH_LIMIT as i64))]
        limit: i64,

        /// Optional text query (matches name, description, tags, metadata)
        #[arg()]
        query: Option<String>,
    },
    /// Execute a single evaluator with canonical test input
    Test {
        /// Fully qualified evaluator identifier (<namespace>/<name>:<version>)
        #[arg()]
        evaluator: String,

        /// Input JSON string
        #[arg(
            long,
            value_name = "JSON",
            conflicts_with = "input_file",
            required_unless_present = "input_file",
            alias = "request"
        )]
        input: Option<String>,

        /// Path to input JSON file
        #[arg(
            long,
            value_name = "FILE",
            value_parser = parse_filepath,
            conflicts_with = "input",
            required_unless_present = "input",
            alias = "request-file"
        )]
        input_file: Option<PathBuf>,
    },
    /// Set evaluator state
    SetState {
        /// Fully qualified evaluator identifier (<namespace>/<name>:<version>)
        #[arg()]
        evaluator: String,

        /// Evaluator state
        #[arg(value_name = "STATE", value_enum)]
        state: EvaluatorState,

        /// Optional reason for setting this state
        #[arg(long, value_name = "TEXT")]
        state_reason: Option<String>,
    },
}

#[async_trait]
impl Executable for SubCommand {
    /// Executes one evaluator operation against runtime + persistence context.
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self {
            SubCommand::Publish {
                evaluator_path,
                release,
                profile,
            } => publish::exec(context, evaluator_path, release, profile).await,
            SubCommand::Show { evaluator } => show::exec(context, evaluator).await,
            SubCommand::Search {
                namespace,
                limit,
                query,
            } => search::exec(context, namespace, limit, query).await,
            SubCommand::Test {
                evaluator,
                input,
                input_file,
            } => test::exec(context, evaluator, input, input_file).await,
            SubCommand::SetState {
                evaluator,
                state,
                state_reason,
            } => set_state::exec(context, evaluator, state, state_reason).await,
        }
    }
}

#[derive(Debug, Args)]
/// Arguments for `vigilo evaluators`.
///
/// If no subcommand is provided, the command lists evaluator versions in the
/// default namespace.
pub(crate) struct Command {
    #[command(subcommand)]
    pub command: Option<SubCommand>,
}

#[async_trait]
impl Executable for Command {
    /// Dispatches evaluator subcommands or falls back to list behavior.
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self.command {
            Some(subcommand) => subcommand.exec(context).await,
            None => {
                let db = context.db().await?.control().await?;
                let evaluators = evaluators::list_evaluators(db, DEFAULT_NAMESPACE).await?;

                if evaluators.is_empty() {
                    warn!("no evaluators found");
                } else {
                    for evaluator in evaluators {
                        info!(
                            "{}/{}:{} state={:?}",
                            evaluator.namespace, evaluator.name, evaluator.version, evaluator.state,
                        );
                    }
                }

                Ok(())
            }
        }
    }
}
