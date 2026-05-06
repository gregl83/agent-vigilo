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
    contracts::evaluator::EvaluatorInput,
    db::tables::evaluators,
    models::evaluator::{
        EvaluatorDraft,
        EvaluatorPatch,
        EvaluatorState,
    },
};

const DEFAULT_NAMESPACE: &str = "vigilo";
const DEFAULT_SEARCH_LIMIT: i64 = 10;
const MAX_SEARCH_LIMIT: i64 = 20;

#[derive(Debug)]
struct EvaluatorIdentity {
    namespace: String,
    name: String,
    version: String,
}

fn parse_fully_qualified_evaluator(input: &str) -> anyhow::Result<EvaluatorIdentity> {
    let (identity, version) = input
        .rsplit_once(':')
        .map(|(l, r)| (l.trim(), r.trim()))
        .ok_or_else(|| anyhow::anyhow!(
            "ambiguous evaluator identifier '{}'; use fully qualified '<namespace>/<name>:<version>'",
            input
        ))?;

    let (namespace, name) = identity
        .rsplit_once('/')
        .map(|(l, r)| (l.trim(), r.trim()))
        .ok_or_else(|| anyhow::anyhow!(
            "ambiguous evaluator identifier '{}'; use fully qualified '<namespace>/<name>:<version>'",
            input
        ))?;

    if namespace.is_empty() || name.is_empty() || version.is_empty() {
        anyhow::bail!(
            "ambiguous evaluator identifier '{}'; use fully qualified '<namespace>/<name>:<version>'",
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
        false => profile.unwrap_or_else(|| "dev".to_string()),
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
    async fn exec(self, context: Context) -> anyhow::Result<()> {
        match self {
            SubCommand::Publish {
                evaluator_path,
                release,
                profile,
            } => {
                info!("publishing evaluator: {}", evaluator_path.display());

                let profile = get_manifest_profile(release, profile);
                let component = context
                    .wasm()
                    .await?
                    .prepare_evaluator(evaluator_path, profile)?;

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
                )
                .await?;

                info!(
                    "successfully published evaluator: {}/{}:{}",
                    evaluator.namespace, evaluator.name, evaluator.version,
                );

                Ok(())
            }
            SubCommand::Show { evaluator } => {
                info!("fetching evaluator {}", evaluator);

                let db = context.db().await?;
                let out = context.out().await?;
                let evaluator = parse_fully_qualified_evaluator(&evaluator)?;

                let evaluator_record = evaluators::select_evaluator(
                    db,
                    &evaluator.namespace,
                    &evaluator.name,
                    &evaluator.version,
                )
                .await?;

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
                            "evaluator not found: {}/{}:{}",
                            evaluator.namespace,
                            evaluator.name,
                            evaluator.version,
                        );
                    }
                };

                out.write_line(serde_json::to_string_pretty(&payload)?)?;

                Ok(())
            }
            SubCommand::Search {
                namespace,
                limit,
                query,
            } => {
                info!(
                    "searching evaluators namespace `{}` for term `{}`",
                    namespace,
                    query.clone().unwrap_or_default(),
                );

                let db = context.db().await?;
                let out = context.out().await?;
                let evaluators =
                    evaluators::search_evaluator_summaries(db, &namespace, query.as_deref(), limit)
                        .await?;

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
            SubCommand::Test {
                evaluator,
                input,
                input_file,
            } => {
                info!("testing evaluator {}", evaluator);

                let db = context.db().await?;
                let out = context.out().await?;
                let wasm = context.wasm().await?;
                let evaluator = parse_fully_qualified_evaluator(&evaluator)?;

                let evaluator_record = evaluators::select_evaluator(
                    db,
                    &evaluator.namespace,
                    &evaluator.name,
                    &evaluator.version,
                )
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "evaluator not found: {}/{}:{}",
                        evaluator.namespace,
                        evaluator.name,
                        evaluator.version,
                    )
                })?;

                match evaluator_record.state {
                    EvaluatorState::Disabled | EvaluatorState::Removed => {
                        anyhow::bail!(
                            "evaluator {}/{}:{} cannot be tested while in state '{}'",
                            evaluator_record.namespace,
                            evaluator_record.name,
                            evaluator_record.version,
                            serde_json::to_string(&evaluator_record.state)?.trim_matches('"'),
                        );
                    }
                    _ => {}
                }

                let input_raw = match (input, input_file) {
                    (Some(raw), None) => raw,
                    (None, Some(path)) => fs::read_to_string(path)?,
                    _ => anyhow::bail!("exactly one of --input or --input-file must be provided"),
                };

                let parsed_input: EvaluatorInput = serde_json::from_str(&input_raw)
                    .map_err(|err| anyhow::anyhow!("invalid evaluator test input json: {}", err))?;

                let evaluation_output =
                    wasm.test_evaluator(&evaluator_record.wasm_bytes, parsed_input)?;

                let normalized_results = evaluation_output.clone().normalize();

                let payload = json!({
                    "data": {
                        "namespace": evaluator_record.namespace,
                        "name": evaluator_record.name,
                        "version": evaluator_record.version,
                        "state": evaluator_record.state,
                        "output": evaluation_output,
                        "normalized_results": normalized_results,
                    }
                });

                out.write_line(serde_json::to_string_pretty(&payload)?)?;

                Ok(())
            }
            SubCommand::SetState {
                evaluator,
                state,
                state_reason,
            } => {
                info!("setting evaluator state {} -> {:?}", evaluator, state);

                let db = context.db().await?;
                let evaluator = parse_fully_qualified_evaluator(&evaluator)?;

                // todo - handle failure reason (e.g. removed -> active failure)
                let affected = evaluators::update_evaluator_state(
                    db,
                    &evaluator.namespace,
                    &evaluator.name,
                    &evaluator.version,
                    &EvaluatorPatch {
                        state: state.clone(),
                        state_reason,
                    },
                )
                .await?;

                if affected == 0 {
                    anyhow::bail!(
                        "failed to set evaluator state {}/{}:{} -> {:?}",
                        evaluator.namespace,
                        evaluator.name,
                        evaluator.version,
                        state,
                    );
                } else {
                    info!(
                        "set evaluator state {}/{}:{} -> {:?}",
                        evaluator.namespace, evaluator.name, evaluator.version, state,
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

#[cfg(test)]
mod tests {
    use super::parse_fully_qualified_evaluator;

    #[test]
    fn parse_fully_qualified_evaluator_accepts_new_format() {
        let parsed = parse_fully_qualified_evaluator("vigilo/sentiment-basic-en:0.1.0").unwrap();

        assert_eq!(parsed.namespace, "vigilo");
        assert_eq!(parsed.name, "sentiment-basic-en");
        assert_eq!(parsed.version, "0.1.0");
    }

    #[test]
    fn parse_fully_qualified_evaluator_rejects_old_format() {
        let err = parse_fully_qualified_evaluator("vigilo:sentiment-basic-en@0.1.0").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("<namespace>/<name>:<version>"));
    }
}
