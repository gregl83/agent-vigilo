//! Run-level aggregation policy for normalized evaluator findings.
//!
//! This module is intentionally persistence-free. Runtime workers pass
//! profile-resolved bindings and normalized findings in, and receive the exact
//! aggregate fields persisted to `execution_aggregates`.

use std::collections::BTreeMap;

use serde_json::json;
use uuid::Uuid;

use crate::contracts::{
    evaluator::EvaluationStatus,
    run::{
        AggregationMethod,
        AggregationSettings,
        DimensionAggregation,
        RunDefaults,
    },
};

#[derive(Debug, Clone)]
pub(crate) struct AggregationFinding {
    pub(crate) evaluator_id: Uuid,
    pub(crate) binding_dimension: String,
    pub(crate) source_dimension: Option<String>,
    pub(crate) status: EvaluationStatus,
    pub(crate) normalized_score: Option<f64>,
    pub(crate) blocking: bool,
    pub(crate) binding_weight: f64,
    pub(crate) failure_category: Option<String>,
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AggregationOutcome {
    pub(crate) overall_status: String,
    pub(crate) aggregate_score: Option<f64>,
    pub(crate) dimension_scores: serde_json::Value,
    pub(crate) blocking_failures: serde_json::Value,
    pub(crate) summary: serde_json::Value,
}

#[derive(Debug)]
struct DimensionAccumulator<'a> {
    policy: DimensionAggregation,
    findings: Vec<(usize, &'a AggregationFinding)>,
}

fn default_dimension_policy() -> DimensionAggregation {
    DimensionAggregation {
        method: AggregationMethod::WeightedMean,
        blocking: false,
        weight: 1.0,
    }
}

pub(crate) fn evaluation_status_key(status: &EvaluationStatus) -> &'static str {
    match status {
        EvaluationStatus::Passed => "passed",
        EvaluationStatus::Failed => "failed",
        EvaluationStatus::Error => "error",
        EvaluationStatus::Skipped => "skipped",
    }
}

fn scoreable_status(status: &EvaluationStatus) -> bool {
    matches!(status, EvaluationStatus::Passed | EvaluationStatus::Failed)
}

fn dimension_score(accumulator: &DimensionAccumulator<'_>) -> Option<f64> {
    let scoreable = accumulator
        .findings
        .iter()
        .filter_map(|(_, finding)| {
            if scoreable_status(&finding.status) {
                finding.normalized_score.map(|score| (*finding, score))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    if scoreable.is_empty() {
        return None;
    }

    match &accumulator.policy.method {
        AggregationMethod::MinScore => Some(
            scoreable
                .into_iter()
                .map(|(_, score)| score)
                .fold(1.0, f64::min),
        ),
        AggregationMethod::WeightedMean => {
            let mut scoreable_count_by_evaluator = BTreeMap::<Uuid, usize>::new();
            for (finding, _) in &scoreable {
                *scoreable_count_by_evaluator
                    .entry(finding.evaluator_id)
                    .or_insert(0) += 1;
            }

            let mut weighted_sum = 0.0;
            let mut weight_sum = 0.0;
            for (finding, score) in scoreable {
                let scoreable_count = *scoreable_count_by_evaluator
                    .get(&finding.evaluator_id)
                    .unwrap_or(&1);
                let contribution_weight = finding.binding_weight / scoreable_count as f64;
                if contribution_weight <= 0.0 {
                    continue;
                }
                weighted_sum += score * contribution_weight;
                weight_sum += contribution_weight;
            }

            if weight_sum > 0.0 {
                Some(weighted_sum / weight_sum)
            } else {
                None
            }
        }
    }
}

pub(crate) fn aggregate_findings(
    defaults: &RunDefaults,
    settings: &AggregationSettings,
    attempt_id: Uuid,
    findings: &[AggregationFinding],
) -> AggregationOutcome {
    let mut dimensions = BTreeMap::<String, DimensionAccumulator<'_>>::new();
    for (index, finding) in findings.iter().enumerate() {
        let policy = settings
            .dimensions
            .get(&finding.binding_dimension)
            .cloned()
            .unwrap_or_else(default_dimension_policy);
        dimensions
            .entry(finding.binding_dimension.clone())
            .or_insert_with(|| DimensionAccumulator {
                policy,
                findings: Vec::new(),
            })
            .findings
            .push((index, finding));
    }

    let mut dimension_scores = serde_json::Map::new();
    let mut weighted_dimension_sum = 0.0;
    let mut dimension_weight_sum = 0.0;
    for (dimension, accumulator) in &dimensions {
        let Some(score) = dimension_score(accumulator) else {
            continue;
        };
        dimension_scores.insert(dimension.clone(), json!(score));
        if accumulator.policy.weight > 0.0 {
            weighted_dimension_sum += score * accumulator.policy.weight;
            dimension_weight_sum += accumulator.policy.weight;
        }
    }

    let aggregate_score = if dimension_weight_sum > 0.0 {
        Some(weighted_dimension_sum / dimension_weight_sum)
    } else {
        None
    };

    let blocking_failures = findings
        .iter()
        .enumerate()
        .filter(|(_, finding)| {
            let dimension_blocking = dimensions
                .get(&finding.binding_dimension)
                .map(|accumulator| accumulator.policy.blocking)
                .unwrap_or(false);
            (finding.blocking || dimension_blocking)
                && matches!(finding.status, EvaluationStatus::Failed | EvaluationStatus::Error)
        })
        .map(|(index, finding)| {
            json!({
                "finding_index": index,
                "evaluator_id": finding.evaluator_id,
                "dimension": finding.binding_dimension.clone(),
                "source_dimension": finding.source_dimension.clone(),
                "status": evaluation_status_key(&finding.status),
                "failure_category": finding.failure_category.clone(),
                "reason": finding.reason.clone(),
            })
        })
        .collect::<Vec<_>>();

    let has_blocking_failure = !blocking_failures.is_empty();
    let has_blocking_error = findings
        .iter()
        .any(|finding| {
            let dimension_blocking = dimensions
                .get(&finding.binding_dimension)
                .map(|accumulator| accumulator.policy.blocking)
                .unwrap_or(false);
            (finding.blocking || dimension_blocking) && finding.status == EvaluationStatus::Error
        });
    let has_scoreable_finding = findings.iter().any(|finding| {
        scoreable_status(&finding.status) && finding.normalized_score.is_some()
    });

    let overall_status = if has_blocking_error {
        "error"
    } else if defaults.fail_on_any_blocking_failure && has_blocking_failure {
        "failed"
    } else if !has_scoreable_finding || aggregate_score.is_none() {
        "error"
    } else if aggregate_score
        .map(|score| score < defaults.min_execution_score)
        .unwrap_or(true)
    {
        "failed"
    } else {
        "passed"
    }
    .to_string();

    let result_status_counts = findings.iter().fold(BTreeMap::new(), |mut counts, finding| {
        *counts
            .entry(evaluation_status_key(&finding.status).to_string())
            .or_insert(0usize) += 1;
        counts
    });

    AggregationOutcome {
        overall_status: overall_status.clone(),
        aggregate_score,
        dimension_scores: serde_json::Value::Object(dimension_scores),
        blocking_failures: serde_json::Value::Array(blocking_failures),
        summary: json!({
            "attempt_id": attempt_id,
            "result_count": findings.len(),
            "overall_status": overall_status,
            "result_status_counts": result_status_counts,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> RunDefaults {
        RunDefaults {
            max_attempts: 1,
            request_timeout_secs: 30,
            fail_on_any_blocking_failure: true,
            min_execution_score: 0.8,
        }
    }

    fn settings(policies: Vec<(&str, AggregationMethod, bool, f64)>) -> AggregationSettings {
        AggregationSettings {
            dimensions: policies
                .into_iter()
                .map(|(dimension, method, blocking, weight)| {
                    (
                        dimension.to_string(),
                        DimensionAggregation {
                            method,
                            blocking,
                            weight,
                        },
                    )
                })
                .collect(),
        }
    }

    fn finding(
        evaluator_id: Uuid,
        dimension: &str,
        status: EvaluationStatus,
        score: Option<f64>,
        blocking: bool,
        weight: f64,
    ) -> AggregationFinding {
        AggregationFinding {
            evaluator_id,
            binding_dimension: dimension.to_string(),
            source_dimension: Some("quality".to_string()),
            status,
            normalized_score: score,
            blocking,
            binding_weight: weight,
            failure_category: None,
            reason: None,
        }
    }

    fn score_for(outcome: &AggregationOutcome, dimension: &str) -> f64 {
        outcome
            .dimension_scores
            .get(dimension)
            .and_then(serde_json::Value::as_f64)
            .unwrap()
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 0.000_001,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn weighted_mean_uses_evaluator_weights() {
        let id_a = Uuid::nil();
        let id_b = Uuid::from_u128(1);
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                false,
                1.0,
            )]),
            Uuid::nil(),
            &[
                finding(id_a, "quality", EvaluationStatus::Passed, Some(1.0), false, 3.0),
                finding(id_b, "quality", EvaluationStatus::Failed, Some(0.0), false, 1.0),
            ],
        );

        assert_close(score_for(&outcome, "quality"), 0.75);
        assert_eq!(outcome.aggregate_score, Some(0.75));
        assert_eq!(outcome.overall_status, "failed");
    }

    #[test]
    fn weighted_mean_divides_binding_weight_across_multiple_findings() {
        let id_a = Uuid::nil();
        let id_b = Uuid::from_u128(1);
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                false,
                1.0,
            )]),
            Uuid::nil(),
            &[
                finding(id_a, "quality", EvaluationStatus::Passed, Some(1.0), false, 1.0),
                finding(id_a, "quality", EvaluationStatus::Failed, Some(0.0), false, 1.0),
                finding(id_b, "quality", EvaluationStatus::Passed, Some(1.0), false, 1.0),
            ],
        );

        assert_close(score_for(&outcome, "quality"), 0.75);
    }

    #[test]
    fn min_score_dimension_uses_lowest_finding_score() {
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![("safety", AggregationMethod::MinScore, true, 1.0)]),
            Uuid::nil(),
            &[
                finding(Uuid::nil(), "safety", EvaluationStatus::Passed, Some(0.9), false, 1.0),
                finding(Uuid::from_u128(1), "safety", EvaluationStatus::Failed, Some(0.2), false, 1.0),
            ],
        );

        assert_close(score_for(&outcome, "safety"), 0.2);
        assert_eq!(outcome.overall_status, "failed");
    }

    #[test]
    fn dimension_weights_control_overall_score() {
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![
                ("quality", AggregationMethod::WeightedMean, false, 3.0),
                ("safety", AggregationMethod::WeightedMean, false, 1.0),
            ]),
            Uuid::nil(),
            &[
                finding(Uuid::nil(), "quality", EvaluationStatus::Passed, Some(1.0), false, 1.0),
                finding(Uuid::from_u128(1), "safety", EvaluationStatus::Failed, Some(0.0), false, 1.0),
            ],
        );

        assert_eq!(outcome.aggregate_score, Some(0.75));
    }

    #[test]
    fn zero_weight_dimension_can_block_without_affecting_score() {
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![
                ("format", AggregationMethod::MinScore, true, 0.0),
                ("quality", AggregationMethod::WeightedMean, false, 1.0),
            ]),
            Uuid::nil(),
            &[
                finding(Uuid::nil(), "format", EvaluationStatus::Failed, Some(0.0), false, 1.0),
                finding(Uuid::from_u128(1), "quality", EvaluationStatus::Passed, Some(1.0), false, 1.0),
            ],
        );

        assert_eq!(outcome.aggregate_score, Some(1.0));
        assert_eq!(outcome.overall_status, "failed");
    }

    #[test]
    fn aggregate_score_at_threshold_passes() {
        let mut defaults = defaults();
        defaults.min_execution_score = 0.75;
        let outcome = aggregate_findings(
            &defaults,
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                false,
                1.0,
            )]),
            Uuid::nil(),
            &[finding(Uuid::nil(), "quality", EvaluationStatus::Passed, Some(0.75), false, 1.0)],
        );

        assert_eq!(outcome.overall_status, "passed");
    }

    #[test]
    fn nonblocking_failed_finding_can_pass_when_score_is_high_enough() {
        let mut defaults = defaults();
        defaults.min_execution_score = 0.7;
        let outcome = aggregate_findings(
            &defaults,
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                false,
                1.0,
            )]),
            Uuid::nil(),
            &[finding(Uuid::nil(), "quality", EvaluationStatus::Failed, Some(0.75), false, 1.0)],
        );

        assert_eq!(outcome.overall_status, "passed");
    }

    #[test]
    fn blocking_failure_does_not_auto_fail_when_gate_is_false() {
        let mut defaults = defaults();
        defaults.fail_on_any_blocking_failure = false;
        defaults.min_execution_score = 0.7;
        let outcome = aggregate_findings(
            &defaults,
            &settings(vec![("quality", AggregationMethod::WeightedMean, true, 1.0)]),
            Uuid::nil(),
            &[finding(Uuid::nil(), "quality", EvaluationStatus::Failed, Some(0.75), true, 1.0)],
        );

        assert_eq!(outcome.overall_status, "passed");
        assert_eq!(outcome.blocking_failures.as_array().unwrap().len(), 1);
    }

    #[test]
    fn blocking_error_returns_error() {
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![("quality", AggregationMethod::WeightedMean, true, 1.0)]),
            Uuid::nil(),
            &[finding(Uuid::nil(), "quality", EvaluationStatus::Error, None, false, 1.0)],
        );

        assert_eq!(outcome.overall_status, "error");
    }

    #[test]
    fn nonblocking_error_is_recorded_but_does_not_auto_error_when_policy_passes() {
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                false,
                1.0,
            )]),
            Uuid::nil(),
            &[
                finding(Uuid::nil(), "quality", EvaluationStatus::Error, None, false, 1.0),
                finding(Uuid::from_u128(1), "quality", EvaluationStatus::Passed, Some(1.0), false, 1.0),
            ],
        );

        assert_eq!(outcome.aggregate_score, Some(1.0));
        assert_eq!(outcome.overall_status, "passed");
    }

    #[test]
    fn skipped_and_informational_findings_do_not_affect_score() {
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                false,
                1.0,
            )]),
            Uuid::nil(),
            &[
                finding(Uuid::nil(), "quality", EvaluationStatus::Skipped, None, false, 1.0),
                finding(Uuid::from_u128(1), "quality", EvaluationStatus::Passed, Some(0.9), false, 1.0),
            ],
        );

        assert_eq!(outcome.aggregate_score, Some(0.9));
        assert_eq!(outcome.overall_status, "passed");
    }

    #[test]
    fn all_unscoreable_findings_returns_error() {
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                false,
                1.0,
            )]),
            Uuid::nil(),
            &[finding(Uuid::nil(), "quality", EvaluationStatus::Skipped, None, false, 1.0)],
        );

        assert_eq!(outcome.aggregate_score, None);
        assert_eq!(outcome.overall_status, "error");
    }

    #[test]
    fn missing_dimension_policy_uses_weighted_mean_default() {
        let outcome = aggregate_findings(
            &defaults(),
            &AggregationSettings {
                dimensions: BTreeMap::new(),
            },
            Uuid::nil(),
            &[finding(Uuid::nil(), "quality", EvaluationStatus::Passed, Some(0.9), false, 1.0)],
        );

        assert_eq!(outcome.aggregate_score, Some(0.9));
    }

    #[test]
    fn binding_dimension_controls_aggregation_bucket() {
        let mut record = finding(
            Uuid::nil(),
            "profile_quality",
            EvaluationStatus::Passed,
            Some(0.9),
            false,
            1.0,
        );
        record.source_dimension = Some("safety".to_string());

        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![(
                "profile_quality",
                AggregationMethod::WeightedMean,
                false,
                1.0,
            )]),
            Uuid::nil(),
            &[record],
        );

        assert!(outcome.dimension_scores.get("profile_quality").is_some());
        assert!(outcome.dimension_scores.get("safety").is_none());
    }

    #[test]
    fn evaluator_emitted_dimension_is_preserved_in_blocking_failure_summary() {
        let mut record = finding(
            Uuid::nil(),
            "profile_safety",
            EvaluationStatus::Failed,
            Some(0.0),
            true,
            1.0,
        );
        record.source_dimension = Some("evaluator_quality".to_string());

        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![("profile_safety", AggregationMethod::MinScore, true, 1.0)]),
            Uuid::nil(),
            &[record],
        );

        assert_eq!(
            outcome.blocking_failures[0]["source_dimension"],
            "evaluator_quality"
        );
    }

    #[test]
    fn multiple_findings_are_all_counted_in_summary() {
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                false,
                1.0,
            )]),
            Uuid::nil(),
            &[
                finding(Uuid::nil(), "quality", EvaluationStatus::Passed, Some(1.0), false, 1.0),
                finding(Uuid::nil(), "quality", EvaluationStatus::Failed, Some(0.0), false, 1.0),
            ],
        );

        assert_eq!(outcome.summary["result_count"], 2);
        assert_eq!(outcome.summary["result_status_counts"]["passed"], 1);
        assert_eq!(outcome.summary["result_status_counts"]["failed"], 1);
    }
}
