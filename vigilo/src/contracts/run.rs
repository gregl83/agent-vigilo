use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

fn default_json_object() -> Value {
    Value::Object(Default::default())
}

/// Run profile used by `vigilo run test`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunProfile {
    pub(crate) profile_id: String,
    pub(crate) profile_version: String,
    pub(crate) description: String,
    pub(crate) defaults: RunDefaults,
    pub(crate) persistence: PersistenceSettings,
    #[serde(default)]
    pub(crate) case_groups: Vec<CaseGroupProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunDefaults {
    pub(crate) max_attempts: u32,
    pub(crate) request_timeout_secs: u32,
    pub(crate) fail_on_any_blocking_failure: bool,
    pub(crate) min_execution_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistenceSettings {
    pub(crate) mode: PersistenceMode,
    pub(crate) persist_raw_outputs: PersistRawOutputsMode,
    pub(crate) persist_evaluator_evidence: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PersistenceMode {
    Full,
    Summary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PersistRawOutputsMode {
    All,
    FailuresOnly,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CaseGroupProfile {
    pub(crate) id: String,
    pub(crate) description: String,
    pub(crate) applies_to: AppliesTo,
    #[serde(default)]
    pub(crate) evaluators: Vec<EvaluatorBinding>,
    pub(crate) aggregation: AggregationSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AppliesTo {
    pub(crate) task_type: String,
    #[serde(default)]
    pub(crate) tags_any: Vec<String>,
    #[serde(default)]
    pub(crate) tags_all: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvaluatorBinding {
    #[serde(rename = "ref")]
    pub(crate) evaluator_ref: String,
    pub(crate) dimension: String,
    pub(crate) blocking: bool,
    pub(crate) weight: f64,
    #[serde(default = "default_json_object")]
    pub(crate) config: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AggregationSettings {
    #[serde(default)]
    pub(crate) dimensions: BTreeMap<String, DimensionAggregation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DimensionAggregation {
    pub(crate) method: AggregationMethod,
    pub(crate) blocking: bool,
    pub(crate) weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AggregationMethod {
    MinScore,
    WeightedMean,
}

/// Dataset envelope used by `vigilo run test`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunDataset {
    #[serde(default)]
    pub(crate) dataset_id: Option<String>,
    #[serde(default)]
    pub(crate) dataset_version: Option<String>,
    #[serde(default)]
    pub(crate) cases: Vec<DatasetCase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DatasetCase {
    pub(crate) id: String,
    pub(crate) task_type: String,
    #[serde(default)]
    pub(crate) case_group: Option<String>,
    pub(crate) input: Value,
    #[serde(default)]
    pub(crate) expected: Option<Value>,
    #[serde(default)]
    pub(crate) context: Option<Value>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
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

