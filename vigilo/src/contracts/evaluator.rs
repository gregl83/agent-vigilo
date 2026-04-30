use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Top-level response returned by one WASM evaluator invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvaluatorResponse {
    /// Machine-readable evaluator identity.
    pub(crate) evaluator: EvaluatorIdentity,

    /// One evaluator may emit one or more findings.
    #[serde(default)]
    pub(crate) results: Vec<EvaluatorFinding>,

    /// Optional evaluator-level metadata, diagnostics, or timing.
    #[serde(default)]
    pub(crate) metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvaluatorIdentity {
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) version: String,

    /// Hash of the WASM artifact or evaluator package.
    pub(crate) content_hash: Option<String>,

    /// WIT/interface version used by this evaluator.
    pub(crate) interface_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvaluatorFinding {
    /// Dimension this finding contributes to.
    pub(crate) dimension: EvaluationDimension,

    /// Evaluator outcome before aggregation.
    pub(crate) status: EvaluationStatus,

    /// Score in evaluator-native form.
    pub(crate) score: Score,

    /// Whether this finding should fail the execution if status is failed/error.
    #[serde(default)]
    pub(crate) blocking: bool,

    /// Optional severity for policy/safety/quality findings.
    pub(crate) severity: Severity,

    /// Optional stable failure category for summaries.
    pub(crate) failure_category: Option<String>,

    /// Human-readable explanation.
    pub(crate) reason: Option<String>,

    /// Structured evidence for debugging/audit.
    #[serde(default)]
    pub(crate) evidence: Value,

    /// Optional labels for reporting.
    #[serde(default)]
    pub(crate) tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum EvaluationDimension {
    Correctness,
    Format,
    Safety,
    Quality,
    Latency,
    ToolUse,
    Calibration,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvaluationStatus {
    Passed,
    Failed,
    Error,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    None,
    Low,
    Medium,
    High,
    Critical,
}

/// Evaluator-native score.
/// The host later normalizes this into 0.0..=1.0.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Score {
    /// Pass/fail result.
    Binary { passed: bool },

    /// Numeric score with explicit scale.
    Range { value: f64, min: f64, max: f64 },

    /// Already normalized score.
    Normalized { value: f64 },

    /// Bucketed severity-like result.
    SeverityMapped { severity: Severity },

    /// Pairwise comparison.
    Preference { outcome: PreferenceOutcome },

    /// Informational finding that should not affect score.
    Informational,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PreferenceOutcome {
    Preferred,
    Tie,
    NotPreferred,
}

/// Host-side normalized result used for aggregation/storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NormalizedEvaluatorResult {
    pub(crate) evaluator_identifier: String,
    pub(crate) dimension: EvaluationDimension,
    pub(crate) status: EvaluationStatus,
    pub(crate) score_kind: String,

    pub(crate) raw_score: Option<f64>,
    pub(crate) raw_score_min: Option<f64>,
    pub(crate) raw_score_max: Option<f64>,

    /// Always 0.0..=1.0 when present.
    pub(crate) normalized_score: Option<f64>,

    pub(crate) blocking: bool,
    pub(crate) severity: Severity,
    pub(crate) failure_category: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) evidence: Value,
    pub(crate) tags: Vec<String>,
}

impl Score {
    pub(crate) fn normalize(&self) -> Option<f64> {
        match self {
            Score::Binary { passed } => Some(if *passed { 1.0 } else { 0.0 }),
            Score::Range { value, min, max } => {
                if max <= min {
                    None
                } else {
                    Some(((value - min) / (max - min)).clamp(0.0, 1.0))
                }
            }
            Score::Normalized { value } => Some(value.clamp(0.0, 1.0)),
            Score::SeverityMapped { severity } => Some(match severity {
                Severity::None => 1.0,
                Severity::Low => 0.75,
                Severity::Medium => 0.5,
                Severity::High => 0.25,
                Severity::Critical => 0.0,
            }),
            Score::Preference { outcome } => Some(match outcome {
                PreferenceOutcome::Preferred => 1.0,
                PreferenceOutcome::Tie => 0.5,
                PreferenceOutcome::NotPreferred => 0.0,
            }),
            Score::Informational => None,
        }
    }

    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Score::Binary { .. } => "binary",
            Score::Range { .. } => "range",
            Score::Normalized { .. } => "normalized",
            Score::SeverityMapped { .. } => "severity_mapped",
            Score::Preference { .. } => "preference",
            Score::Informational => "informational",
        }
    }

    pub(crate) fn raw_parts(&self) -> (Option<f64>, Option<f64>, Option<f64>) {
        match self {
            Score::Binary { passed } => {
                let raw = if *passed { 1.0 } else { 0.0 };
                (Some(raw), Some(0.0), Some(1.0))
            }
            Score::Range { value, min, max } => (Some(*value), Some(*min), Some(*max)),
            Score::Normalized { value } => (Some(*value), Some(0.0), Some(1.0)),
            Score::SeverityMapped { .. } => (None, None, None),
            Score::Preference { outcome } => {
                let raw = match outcome {
                    PreferenceOutcome::Preferred => 1.0,
                    PreferenceOutcome::Tie => 0.5,
                    PreferenceOutcome::NotPreferred => 0.0,
                };
                (Some(raw), Some(0.0), Some(1.0))
            }
            Score::Informational => (None, None, None),
        }
    }
}

impl EvaluatorResponse {
    pub(crate) fn normalize(self) -> Vec<NormalizedEvaluatorResult> {
        let evaluator_identifier = format!(
            "{}:{}@{}",
            self.evaluator.namespace, self.evaluator.name, self.evaluator.version
        );

        self.results
            .into_iter()
            .map(|finding| {
                let (raw_score, raw_score_min, raw_score_max) = finding.score.raw_parts();

                NormalizedEvaluatorResult {
                    evaluator_identifier: evaluator_identifier.clone(),
                    dimension: finding.dimension,
                    status: finding.status,
                    score_kind: finding.score.kind().to_string(),
                    raw_score,
                    raw_score_min,
                    raw_score_max,
                    normalized_score: finding.score.normalize(),
                    blocking: finding.blocking,
                    severity: finding.severity,
                    failure_category: finding.failure_category,
                    reason: finding.reason,
                    evidence: finding.evidence,
                    tags: finding.tags,
                }
            })
            .collect()
    }
}

/// Aggregation-ready grouping helper.
pub(crate) fn group_by_dimension(
    results: &[NormalizedEvaluatorResult],
) -> BTreeMap<EvaluationDimension, Vec<&NormalizedEvaluatorResult>> {
    let mut grouped = BTreeMap::new();

    for result in results {
        grouped
            .entry(result.dimension.clone())
            .or_insert_with(Vec::new)
            .push(result);
    }

    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_range_returns_none_for_invalid_bounds() {
        let score = Score::Range {
            value: 1.0,
            min: 10.0,
            max: 5.0,
        };

        assert_eq!(score.normalize(), None);
    }

    #[test]
    fn response_normalize_uses_identifier_format() {
        let response = EvaluatorResponse {
            evaluator: EvaluatorIdentity {
                namespace: "vigilo".to_string(),
                name: "sentiment".to_string(),
                version: "0.1.0".to_string(),
                content_hash: None,
                interface_version: None,
            },
            results: vec![EvaluatorFinding {
                dimension: EvaluationDimension::Quality,
                status: EvaluationStatus::Passed,
                score: Score::Normalized { value: 0.8 },
                blocking: false,
                severity: Severity::None,
                failure_category: None,
                reason: None,
                evidence: Value::Null,
                tags: vec![],
            }],
            metadata: Value::Null,
        };

        let normalized = response.normalize();
        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].evaluator_identifier, "vigilo:sentiment@0.1.0");
        assert_eq!(normalized[0].normalized_score, Some(0.8));
    }
}
