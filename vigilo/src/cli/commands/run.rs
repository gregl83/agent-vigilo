use async_trait::async_trait;
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use tracing::info;

use super::Executable;
use super::args::parsers::parse_filepath;
use crate::context::Context;
use crate::contracts::run::{RunDataset, RunProfile};

fn read_inline_or_file(
	inline: Option<String>,
	file: Option<PathBuf>,
	field: &str,
) -> anyhow::Result<String> {
	match (inline, file) {
		(Some(raw), None) => Ok(raw),
		(None, Some(path)) => fs::read_to_string(path)
			.map_err(|err| anyhow::anyhow!("failed to read {} file: {}", field, err)),
		_ => anyhow::bail!("exactly one of --{} or --{}-file must be provided", field, field),
	}
}

fn parse_structured_payload(raw: &str, field: &str) -> anyhow::Result<Value> {
	serde_yaml::from_str::<Value>(raw)
		.map_err(|err| anyhow::anyhow!("invalid {} payload (yaml/json expected): {}", field, err))
}

#[derive(Debug, Subcommand)]
pub(crate) enum SubCommand {
	/// Parse and validate run profile + dataset inputs
	Test {
		/// Run profile YAML/JSON inline string
		#[arg(
			long,
			value_name = "YAML_OR_JSON",
			conflicts_with = "profile_file",
			required_unless_present = "profile_file"
		)]
		profile: Option<String>,

		/// Path to run profile YAML/JSON file
		#[arg(
			long,
			value_name = "FILE",
			value_parser = parse_filepath,
			conflicts_with = "profile",
			required_unless_present = "profile"
		)]
		profile_file: Option<PathBuf>,

		/// Dataset YAML/JSON inline string
		#[arg(
			long,
			value_name = "YAML_OR_JSON",
			conflicts_with = "dataset_file",
			required_unless_present = "dataset_file"
		)]
		dataset: Option<String>,

		/// Path to dataset YAML/JSON file
		#[arg(
			long,
			value_name = "FILE",
			value_parser = parse_filepath,
			conflicts_with = "dataset",
			required_unless_present = "dataset"
		)]
		dataset_file: Option<PathBuf>,
	},

	/// Watch run progress and stream status updates
	Watch {
		/// Run identifier to watch
		run_id: String,
	},
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
			Some(SubCommand::Watch { run_id }) => {
				info!("watching run {}", run_id);

				// TODO: Poll run/execution state changes with backoff until terminal state.
				// TODO: Stream incremental progress snapshots to output context.
				// TODO: Add optional follow mode and structured event output.
				anyhow::bail!("run watch is not implemented yet")
			}
			Some(SubCommand::Test {
				profile,
				profile_file,
				dataset,
				dataset_file,
			}) => {
				info!("parsing run test profile and dataset inputs");

				let out = context.out().await?;
				let profile_raw = read_inline_or_file(profile, profile_file, "profile")?;
				let dataset_raw = read_inline_or_file(dataset, dataset_file, "dataset")?;

				let profile_payload = parse_structured_payload(&profile_raw, "profile")?;
				let dataset_payload = parse_structured_payload(&dataset_raw, "dataset")?;

				let parsed_profile: RunProfile = serde_yaml::from_str(&profile_raw)
					.map_err(|err| anyhow::anyhow!("invalid profile schema: {}", err))?;
				let parsed_dataset: RunDataset = serde_yaml::from_str(&dataset_raw)
					.map_err(|err| anyhow::anyhow!("invalid dataset schema: {}", err))?;

				let payload = json!({
					"data": {
						"profile": parsed_profile,
						"dataset": parsed_dataset,
					},
					"meta": {
						"profile_case_groups": parsed_profile.case_groups.len(),
						"dataset_cases": parsed_dataset.cases.len(),
						"sources": {
							"profile": if profile_payload.is_object() || profile_payload.is_array() { "structured" } else { "scalar" },
							"dataset": if dataset_payload.is_object() || dataset_payload.is_array() { "structured" } else { "scalar" },
						}
					}
				});

				out.write_line(serde_json::to_string_pretty(&payload)?)?;
				Ok(())
			}
			None => anyhow::bail!(
				"missing run subcommand; use `vigilo run test --profile-file <file> --dataset-file <file>`"
			),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{parse_structured_payload, read_inline_or_file};

	#[test]
	fn read_inline_or_file_prefers_inline() {
		let raw = read_inline_or_file(Some("k: v".to_string()), None, "profile").unwrap();
		assert_eq!(raw, "k: v");
	}

	#[test]
	fn parse_structured_payload_accepts_yaml_and_json() {
		let yaml = parse_structured_payload("a: 1", "profile").unwrap();
		assert_eq!(yaml.get("a").and_then(|v| v.as_i64()), Some(1));

		let json = parse_structured_payload("{\"a\":1}", "dataset").unwrap();
		assert_eq!(json.get("a").and_then(|v| v.as_i64()), Some(1));
	}
}

