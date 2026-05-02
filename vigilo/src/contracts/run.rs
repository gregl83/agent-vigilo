//! Contracts used at the run-configuration and dataset input boundary.
//!
//! These types define the canonical profile and dataset payloads consumed by
//! `vigilo run test`. They are intentionally transport-focused contracts used
//! for parsing and validation before orchestration/runtime execution logic.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

fn default_json_object() -> Value {
    Value::Object(Default::default())
}

/// Run profile used by `vigilo run test`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunProfile {
    /// Stable profile identifier (for example, `mixed_agent_release`).
    ///
    /// This is intended to be human-meaningful and stable across revisions.
    pub(crate) profile_id: String,

    /// Profile document version (independent from evaluator versions).
    ///
    /// Allows evolving profile behavior/configuration over time.
    pub(crate) profile_version: String,

    /// Human-readable summary of profile purpose and scope.
    pub(crate) description: String,

    /// Default runtime policy values applied during run orchestration.
    pub(crate) defaults: RunDefaults,

    /// Persistence behavior controls for run/evaluation artifacts.
    pub(crate) persistence: PersistenceSettings,

    /// Case-group specific evaluator bindings and aggregation policies.
    ///
    /// Empty by default to allow incremental authoring/validation.
    #[serde(default)]
    pub(crate) case_groups: Vec<CaseGroupProfile>,
}

/// Default execution policy for run processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunDefaults {
    /// Maximum number of attempts per execution before terminal failure.
    pub(crate) max_attempts: u32,

    /// Request timeout budget, in seconds, for invocation/evaluation phases.
    pub(crate) request_timeout_secs: u32,

    /// Whether any blocking evaluator failure should fail the full execution.
    pub(crate) fail_on_any_blocking_failure: bool,

    /// Minimum aggregate execution score required for passing policy.
    pub(crate) min_execution_score: f64,
}

/// Persistence policy controls for run output and evidence retention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistenceSettings {
    /// High-level persistence strategy (`full` vs `summary`).
    pub(crate) mode: PersistenceMode,

    /// Raw output retention policy (`all`, `failures_only`, `none`).
    pub(crate) persist_raw_outputs: PersistRawOutputsMode,

    /// Whether evaluator evidence blobs should be retained.
    pub(crate) persist_evaluator_evidence: bool,
}

/// Persistence breadth mode for run artifacts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PersistenceMode {
    /// Persist complete run/evaluator detail.
    Full,

    /// Persist reduced summary-level data only.
    Summary,
}

/// Raw-output retention strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PersistRawOutputsMode {
    /// Persist raw outputs for every case/execution.
    All,

    /// Persist raw outputs only for failed/blocking outcomes.
    FailuresOnly,

    /// Do not persist raw outputs.
    None,
}

/// Profile block that binds case selection, evaluators, and aggregation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CaseGroupProfile {
    /// Stable identifier for this case group.
    pub(crate) id: String,

    /// Human-readable case-group description.
    pub(crate) description: String,

    /// Case-matching selector for applying this group.
    pub(crate) applies_to: AppliesTo,

    /// Evaluator bindings used when a case matches `applies_to`.
    #[serde(default)]
    pub(crate) evaluators: Vec<EvaluatorBinding>,

    /// Dimension-level aggregation strategy for this group.
    pub(crate) aggregation: AggregationSettings,
}

/// Selector used to determine which dataset cases this profile group applies to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AppliesTo {
    /// Primary task type discriminator (for example, `classification`).
    pub(crate) task_type: String,

    /// Optional tag OR-filter; at least one tag should match when provided.
    #[serde(default)]
    pub(crate) tags_any: Vec<String>,

    /// Optional tag AND-filter; all listed tags should match when provided.
    #[serde(default)]
    pub(crate) tags_all: Vec<String>,
}

/// Evaluator binding configuration for one case-group entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvaluatorBinding {
    /// Evaluator reference string as declared in profile payload.
    ///
    /// This is currently treated as an opaque identifier at parse time.
    #[serde(rename = "ref")]
    pub(crate) evaluator_ref: String,

    /// Aggregation/reporting dimension this evaluator contributes to.
    pub(crate) dimension: String,

    /// Whether this evaluator can act as a hard gate.
    pub(crate) blocking: bool,

    /// Relative weighting for this evaluator within its dimension.
    pub(crate) weight: f64,

    /// Evaluator-specific unstructured configuration payload.
    #[serde(default = "default_json_object")]
    pub(crate) config: Value,
}

/// Aggregation policy for a case-group, keyed by dimension name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AggregationSettings {
    /// Dimension aggregation rules (`format`, `correctness`, `safety`, etc.).
    #[serde(default)]
    pub(crate) dimensions: BTreeMap<String, DimensionAggregation>,
}

/// Aggregation strategy for one dimension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DimensionAggregation {
    /// Scoring method used to combine evaluator outputs.
    pub(crate) method: AggregationMethod,

    /// Whether this dimension can fail execution/run gating.
    pub(crate) blocking: bool,

    /// Relative dimension contribution to overall score.
    pub(crate) weight: f64,
}

/// Supported dimension aggregation methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AggregationMethod {
    /// Use the minimum score among contributors.
    MinScore,

    /// Use a weighted arithmetic mean.
    WeightedMean,
}

/// Dataset envelope used by `vigilo run test`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunDataset {
    /// Optional stable dataset identifier.
    #[serde(default)]
    pub(crate) dataset_id: Option<String>,

    /// Optional dataset version string.
    #[serde(default)]
    pub(crate) dataset_version: Option<String>,

    /// Dataset cases included in this payload.
    #[serde(default)]
    pub(crate) cases: Vec<DatasetCase>,
}

/// Single dataset case consumed by run/profile matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DatasetCase {
    /// Stable case identifier in dataset.
    pub(crate) id: String,

    /// Task type used for profile `applies_to` matching.
    pub(crate) task_type: String,

    /// Optional pre-assigned case group label.
    #[serde(default)]
    pub(crate) case_group: Option<String>,

    /// Input payload sent to target system/agent.
    pub(crate) input: Value,

    /// Optional expected/oracle payload used by evaluators.
    #[serde(default)]
    pub(crate) expected: Option<Value>,

    /// Optional supporting context payload.
    #[serde(default)]
    pub(crate) context: Option<Value>,

    /// Optional tags for filtering/routing/reporting.
    #[serde(default)]
    pub(crate) tags: Vec<String>,

    /// Optional arbitrary bookkeeping metadata.
    #[serde(default)]
    pub(crate) metadata: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_profile_draft_yaml() {
        let raw = r#"
profile_id: mixed_agent_release
profile_version: 1.0.0
description: Release-grade evaluation profile for mixed generative AI agent tasks.
defaults:
  max_attempts: 2
  request_timeout_secs: 60
  fail_on_any_blocking_failure: true
  min_execution_score: 0.85
persistence:
  mode: full
  persist_raw_outputs: failures_only
  persist_evaluator_evidence: true
case_groups:
  - id: classification
    description: Evaluates classification-style cases.
    applies_to:
      task_type: classification
    evaluators:
      - ref: core/json-schema@1.0.0
        dimension: format
        blocking: true
        weight: 1.0
        config:
          schema:
            type: object
    aggregation:
      dimensions:
        format:
          method: min_score
          blocking: true
          weight: 0.0
"#;

        let profile: RunProfile = serde_yaml::from_str(raw).unwrap();
        assert_eq!(profile.profile_id, "mixed_agent_release");
        assert_eq!(profile.case_groups.len(), 1);
        assert_eq!(
            profile.case_groups[0].evaluators[0].evaluator_ref,
            "core/json-schema@1.0.0"
        );
    }

    #[test]
    fn parse_dataset_yaml() {
        let raw = r#"
dataset_id: sample-dataset
dataset_version: 1.0.0
cases:
  - id: sentiment_001
    task_type: classification
    case_group: classification
    input:
      user_message: "I love this product"
    expected:
      label: positive
    tags: [smoke]
    metadata:
      source: synthetic
"#;

        let dataset: RunDataset = serde_yaml::from_str(raw).unwrap();
        assert_eq!(dataset.dataset_id.as_deref(), Some("sample-dataset"));
        assert_eq!(dataset.cases.len(), 1);
        assert_eq!(dataset.cases[0].task_type, "classification");
    }
}

