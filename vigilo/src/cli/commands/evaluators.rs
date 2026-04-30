use std::path::PathBuf;
use async_trait::async_trait;
use clap::{
    Args,
    Subcommand,
};
use serde_json::json;
use tracing::{info, warn};

use crate::context::Context;
use crate::db::evaluators;
use crate::models::evaluator::{
    EvaluatorDraft,
    EvaluatorPatch,
    EvaluatorState,
};
use super::args::parsers::parse_dir;
use super::Executable;

const DEFAULT_NAMESPACE: &str = "vigilo";
const DEFAULT_SEARCH_LIMIT: i64 = 10;
const MAX_SEARCH_LIMIT: i64 = 20;

struct EvaluatorIdentity {
    namespace: String,
    name: String,
    version: String,
}

fn parse_fully_qualified_evaluator(input: &str) -> anyhow::Result<EvaluatorIdentity> {
    let (identity, version) = input
        .split_once('@')
        .map(|(l, r)| (l.trim(), r.trim()))
        .ok_or_else(|| anyhow::anyhow!(
            "ambiguous evaluator reference '{}'; use fully qualified '<namespace>:<name>@<version>'",
            input
        ))?;

    let (namespace, name) = identity
        .split_once(':')
        .map(|(l, r)| (l.trim(), r.trim()))
        .ok_or_else(|| anyhow::anyhow!(
            "ambiguous evaluator reference '{}'; use fully qualified '<namespace>:<name>@<version>'",
            input
        ))?;

    if namespace.is_empty() || name.is_empty() || version.is_empty() {
        anyhow::bail!(
            "ambiguous evaluator reference '{}'; use fully qualified '<namespace>:<name>@<version>'",
            input
        );
    }

    Ok(EvaluatorIdentity {
        namespace: namespace.to_string(),
        name: name.to_string(),
        version: version.to_string(),
    })
}


fn get_manifest_profile(release: bool, profile: Option<String>) -> String {
    match release {
        true => "release".to_string(),
        false => profile.unwrap_or_else(|| "dev".to_string())
    }
}

#[derive(Debug, Subcommand)]
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
        /// Fully qualified evaluator reference (<namespace>:<name>@<version>)
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
    /// Set evaluator state
    SetState {
        /// Fully qualified evaluator reference (<namespace>:<name>@<version>)
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
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self {
            SubCommand::Publish{ evaluator_path, release, profile } => {
                info!("publishing evaluator: {}", evaluator_path.display());

                let profile = get_manifest_profile(release, profile);
                let component = context.wasm().await?.prepare_component(
                    evaluator_path,
                    profile,
                )?;

                let db = context.db().await?;
                let evaluator = evaluators::insert_evaluator(
                    db,
                    &EvaluatorDraft {
                        namespace: DEFAULT_NAMESPACE.to_string(),
                        name: component.name,
                        version: component.version,
                        content_hash: component.wasm_hash,
                        wasm_bytes: component.wasm_bytes,
                        interface_name: component.interface_name,
                        interface_version: component.interface_version,
                        wit_world: component.wit_world,
                        runtime: component.runtime,
                        runtime_version: component.runtime_version,
                        runtime_fingerprint: component.runtime_fingerprint,
                        description: component.description,
                        tags: component.tags,
                        metadata: component.metadata,
                    },
                ).await?;

                info!(
                    "successfully published evaluator: {}:{}@{}",
                    evaluator.namespace,
                    evaluator.name,
                    evaluator.version,
                );

                Ok(())
            }
            SubCommand::Show{ evaluator } => {
                info!("fetching evaluator {}", evaluator);

                let db = context.db().await?;
                let out = context.out().await?;
                let evaluator = parse_fully_qualified_evaluator(&evaluator)?;

                let evaluator_record = evaluators::select_evaluator(
                    db,
                    &evaluator.namespace,
                    &evaluator.name,
                    &evaluator.version,
                ).await?;

                let payload = match evaluator_record {
                    Some(e) => json!({
                        "data": {
                            "id": e.id,
                            "namespace": e.namespace,
                            "name": e.name,
                            "version": e.version,
                            "content_hash": e.content_hash,
                            "wasm_size_bytes": e.wasm_size_bytes,
                            "interface_name": e.interface_name,
                            "interface_version": e.interface_version,
                            "wit_world": e.wit_world,
                            "runtime": e.runtime,
                            "runtime_version": e.runtime_version,
                            "runtime_fingerprint": e.runtime_fingerprint,
                            "description": e.description,
                            "tags": e.tags,
                            "metadata": e.metadata,
                            "state": e.state,
                            "state_reason": e.state_reason,
                            "created_at": e.created_at,
                            "updated_at": e.updated_at,
                        }
                    }),
                    None => {
                        anyhow::bail!(
                            "evaluator not found: {}:{}@{}",
                            evaluator.namespace,
                            evaluator.name,
                            evaluator.version,
                        );
                    },
                };

                out.write_line(serde_json::to_string_pretty(&payload)?)?;

                Ok(())
            }
            SubCommand::Search { namespace, limit, query } => {
                info!(
                    "searching evaluators namespace `{}` for term `{}`",
                    namespace,
                    query.clone().unwrap_or_default(),
                );

                let db = context.db().await?;
                let out = context.out().await?;
                let evaluators = evaluators::search_evaluator_summaries(
                    db,
                    &namespace,
                    query.as_deref(),
                    limit,
                ).await?;

                let payload = json!({
                    "data": evaluators,
                    "meta": {
                        "namespace": namespace,
                        "query": query,
                        "limit": limit,
                        "count": evaluators.len(),
                    },
                });

                out.write_line(serde_json::to_string_pretty(&payload)?)?;

                Ok(())
            }
            SubCommand::SetState{ evaluator, state, state_reason } => {
                info!("setting evaluator state {} -> {:?}", evaluator, state);

                let db = context.db().await?;
                let evaluator = parse_fully_qualified_evaluator(&evaluator)?;

                let affected = evaluators::update_evaluator_state(
                    db,
                    &evaluator.namespace,
                    &evaluator.name,
                    &evaluator.version,
                    &EvaluatorPatch { state: state.clone(), state_reason },
                ).await?;

                if affected == 0 {
                    anyhow::bail!(
                        "failed to set evaluator state {}:{}@{} -> {:?}",
                        evaluator.namespace,
                        evaluator.name,
                        evaluator.version,
                        state,
                    );
                } else {
                    info!(
                        "set evaluator state {}:{}@{} -> {:?}",
                        evaluator.namespace,
                        evaluator.name,
                        evaluator.version,
                        state,
                    );
                }


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
                    warn!("no evaluators found");
                } else {
                    for evaluator in evaluators {
                        info!(
                            "{}:{}@{} state={:?}",
                            evaluator.namespace,
                            evaluator.name,
                            evaluator.version,
                            evaluator.state,
                        );
                    }
                }

                Ok(())
            }
        }
    }
}
