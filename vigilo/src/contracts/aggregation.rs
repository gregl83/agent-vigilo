//! Run-level aggregation policy for host-normalized evaluator invocation results.
//!
//! This module is intentionally persistence-free. Runtime workers pass
//! profile-resolved bindings and normalized results in, and receive the exact
//! completeness and aggregate fields persisted to `execution_aggregates`.

use std::collections::{
    BTreeMap,
    BTreeSet,
};

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
pub(crate) struct AggregationBinding {
    pub(crate) binding_id: String,
    pub(crate) required: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct AggregationResult {
    pub(crate) binding_id: String,
    pub(crate) evaluator_id: Uuid,
    pub(crate) binding_dimension: String,
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
    results: Vec<(usize, &'a AggregationResult)>,
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
        EvaluationStatus::Abstained => "abstained",
    }
}

fn scoreable_status(status: &EvaluationStatus) -> bool {
    matches!(status, EvaluationStatus::Passed | EvaluationStatus::Failed)
}

fn valid_normalized_score(result: &AggregationResult) -> bool {
    scoreable_status(&result.status)
        && result
            .normalized_score
            .is_some_and(|score| score.is_finite() && (0.0..=1.0).contains(&score))
}

fn evaluator_completeness(
    bindings: &[AggregationBinding],
    results: &[AggregationResult],
) -> (bool, serde_json::Value, BTreeSet<String>) {
    let mut expected = BTreeMap::new();
    let mut duplicate_binding_count = 0usize;
    for binding in bindings {
        if expected
            .insert(binding.binding_id.clone(), binding.required)
            .is_some()
        {
            duplicate_binding_count += 1;
        }
    }

    let mut results_by_binding = BTreeMap::<String, Vec<&AggregationResult>>::new();
    for result in results {
        results_by_binding
            .entry(result.binding_id.clone())
            .or_default()
            .push(result);
    }
    let duplicate_result_count = results_by_binding
        .values()
        .map(|results| results.len().saturating_sub(1))
        .sum::<usize>();

    let required_binding_ids = expected
        .iter()
        .filter_map(|(binding_id, required)| required.then_some(binding_id.clone()))
        .collect::<BTreeSet<_>>();
    let unexpected_binding_ids = results_by_binding
        .keys()
        .filter(|binding_id| !expected.contains_key(*binding_id))
        .cloned()
        .collect::<Vec<_>>();
    let mut required_scored_count = 0usize;
    let mut required_error_count = 0usize;
    let mut required_abstained_count = 0usize;
    let mut required_missing_count = 0usize;
    let mut required_unscored_count = 0usize;
    let mut incomplete_evaluator_ids = Vec::new();

    for binding_id in &required_binding_ids {
        let Some(binding_results) = results_by_binding.get(binding_id) else {
            required_missing_count += 1;
            incomplete_evaluator_ids.push(binding_id.clone());
            continue;
        };

        if binding_results.len() != 1 {
            required_unscored_count += 1;
            incomplete_evaluator_ids.push(binding_id.clone());
            continue;
        }

        if binding_results
            .iter()
            .any(|finding| finding.status == EvaluationStatus::Error)
        {
            required_error_count += 1;
            incomplete_evaluator_ids.push(binding_id.clone());
        } else if binding_results
            .iter()
            .any(|finding| finding.status == EvaluationStatus::Abstained)
        {
            required_abstained_count += 1;
            incomplete_evaluator_ids.push(binding_id.clone());
        } else if binding_results
            .iter()
            .any(|finding| valid_normalized_score(finding))
        {
            required_scored_count += 1;
        } else {
            required_unscored_count += 1;
            incomplete_evaluator_ids.push(binding_id.clone());
        }
    }

    let complete = !required_binding_ids.is_empty()
        && duplicate_binding_count == 0
        && duplicate_result_count == 0
        && unexpected_binding_ids.is_empty()
        && required_scored_count == required_binding_ids.len();
    let summary = json!({
        "complete": complete,
        "expected_binding_count": bindings.len(),
        "required_binding_count": required_binding_ids.len(),
        "optional_binding_count": expected.len() - required_binding_ids.len(),
        "required_scored_count": required_scored_count,
        "required_error_count": required_error_count,
        "required_abstained_count": required_abstained_count,
        "required_missing_count": required_missing_count,
        "required_unscored_count": required_unscored_count,
        "duplicate_binding_count": duplicate_binding_count,
        "duplicate_result_count": duplicate_result_count,
        "unexpected_binding_count": unexpected_binding_ids.len(),
        "incomplete_binding_ids": incomplete_evaluator_ids,
        "unexpected_binding_ids": unexpected_binding_ids,
    });

    (complete, summary, required_binding_ids)
}

fn dimension_score(accumulator: &DimensionAccumulator<'_>) -> Option<f64> {
    let scoreable = accumulator
        .results
        .iter()
        .filter_map(|(_, finding)| {
            if valid_normalized_score(finding) {
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
            let mut weighted_sum = 0.0;
            let mut weight_sum = 0.0;
            for (finding, score) in scoreable {
                let contribution_weight = finding.binding_weight;
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

pub(crate) fn aggregate_results(
    defaults: &RunDefaults,
    settings: &AggregationSettings,
    attempt_id: Uuid,
    bindings: &[AggregationBinding],
    results: &[AggregationResult],
) -> AggregationOutcome {
    let (completeness_is_valid, completeness, required_binding_ids) =
        evaluator_completeness(bindings, results);
    let mut dimensions = BTreeMap::<String, DimensionAccumulator<'_>>::new();
    for (index, result) in results.iter().enumerate() {
        if !required_binding_ids.contains(&result.binding_id) {
            continue;
        }
        let policy = settings
            .dimensions
            .get(&result.binding_dimension)
            .cloned()
            .unwrap_or_else(default_dimension_policy);
        dimensions
            .entry(result.binding_dimension.clone())
            .or_insert_with(|| DimensionAccumulator {
                policy,
                results: Vec::new(),
            })
            .results
            .push((index, result));
    }

    let mut dimension_scores = serde_json::Map::new();
    let mut weighted_dimension_sum = 0.0;
    let mut dimension_weight_sum = 0.0;
    if completeness_is_valid {
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
    }

    let aggregate_score = if dimension_weight_sum > 0.0 {
        Some(weighted_dimension_sum / dimension_weight_sum)
    } else {
        None
    };

    let blocking_failures = results
        .iter()
        .enumerate()
        .filter(|(_, finding)| {
            if !required_binding_ids.contains(&finding.binding_id) {
                return false;
            }
            let dimension_blocking = dimensions
                .get(&finding.binding_dimension)
                .map(|accumulator| accumulator.policy.blocking)
                .unwrap_or(false);
            (finding.blocking || dimension_blocking)
                && matches!(
                    finding.status,
                    EvaluationStatus::Failed | EvaluationStatus::Error
                )
        })
        .map(|(index, finding)| {
            json!({
                "result_index": index,
                "binding_id": finding.binding_id,
                "evaluator_id": finding.evaluator_id,
                "dimension": finding.binding_dimension.clone(),
                "status": evaluation_status_key(&finding.status),
                "failure_category": finding.failure_category.clone(),
                "reason": finding.reason.clone(),
            })
        })
        .collect::<Vec<_>>();

    let has_blocking_failure = !blocking_failures.is_empty();
    let has_scoreable_result = results.iter().any(|result| {
        required_binding_ids.contains(&result.binding_id) && valid_normalized_score(result)
    });

    let overall_status = if !completeness_is_valid {
        "error"
    } else if defaults.fail_on_any_blocking_failure && has_blocking_failure {
        "failed"
    } else if !has_scoreable_result || aggregate_score.is_none() {
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

    let result_status_counts = results.iter().fold(BTreeMap::new(), |mut counts, finding| {
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
            "result_count": results.len(),
            "overall_status": overall_status,
            "result_status_counts": result_status_counts,
            "evaluator_completeness": completeness,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aggregate_findings(
        defaults: &RunDefaults,
        settings: &AggregationSettings,
        attempt_id: Uuid,
        findings: &[AggregationResult],
    ) -> AggregationOutcome {
        let bindings = findings
            .iter()
            .map(|finding| finding.evaluator_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|evaluator_id| AggregationBinding {
                binding_id: evaluator_id.to_string(),
                required: true,
            })
            .collect::<Vec<_>>();
        super::aggregate_results(defaults, settings, attempt_id, &bindings, findings)
    }

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
    ) -> AggregationResult {
        AggregationResult {
            binding_id: evaluator_id.to_string(),
            evaluator_id,
            binding_dimension: dimension.to_string(),
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
                finding(
                    id_a,
                    "quality",
                    EvaluationStatus::Passed,
                    Some(1.0),
                    false,
                    3.0,
                ),
                finding(
                    id_b,
                    "quality",
                    EvaluationStatus::Failed,
                    Some(0.0),
                    false,
                    1.0,
                ),
            ],
        );

        assert_close(score_for(&outcome, "quality"), 0.75);
        assert_eq!(outcome.aggregate_score, Some(0.75));
        assert_eq!(outcome.overall_status, "failed");
    }

    #[test]
    fn min_score_dimension_uses_lowest_finding_score() {
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![("safety", AggregationMethod::MinScore, true, 1.0)]),
            Uuid::nil(),
            &[
                finding(
                    Uuid::nil(),
                    "safety",
                    EvaluationStatus::Passed,
                    Some(0.9),
                    false,
                    1.0,
                ),
                finding(
                    Uuid::from_u128(1),
                    "safety",
                    EvaluationStatus::Failed,
                    Some(0.2),
                    false,
                    1.0,
                ),
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
                finding(
                    Uuid::nil(),
                    "quality",
                    EvaluationStatus::Passed,
                    Some(1.0),
                    false,
                    1.0,
                ),
                finding(
                    Uuid::from_u128(1),
                    "safety",
                    EvaluationStatus::Failed,
                    Some(0.0),
                    false,
                    1.0,
                ),
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
                finding(
                    Uuid::nil(),
                    "format",
                    EvaluationStatus::Failed,
                    Some(0.0),
                    false,
                    1.0,
                ),
                finding(
                    Uuid::from_u128(1),
                    "quality",
                    EvaluationStatus::Passed,
                    Some(1.0),
                    false,
                    1.0,
                ),
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
            &[finding(
                Uuid::nil(),
                "quality",
                EvaluationStatus::Passed,
                Some(0.75),
                false,
                1.0,
            )],
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
            &[finding(
                Uuid::nil(),
                "quality",
                EvaluationStatus::Failed,
                Some(0.75),
                false,
                1.0,
            )],
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
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                true,
                1.0,
            )]),
            Uuid::nil(),
            &[finding(
                Uuid::nil(),
                "quality",
                EvaluationStatus::Failed,
                Some(0.75),
                true,
                1.0,
            )],
        );

        assert_eq!(outcome.overall_status, "passed");
        assert_eq!(outcome.blocking_failures.as_array().unwrap().len(), 1);
    }

    #[test]
    fn blocking_error_returns_error() {
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                true,
                1.0,
            )]),
            Uuid::nil(),
            &[finding(
                Uuid::nil(),
                "quality",
                EvaluationStatus::Error,
                None,
                false,
                1.0,
            )],
        );

        assert_eq!(outcome.overall_status, "error");
    }

    #[test]
    fn required_nonblocking_error_withholds_scores() {
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
                finding(
                    Uuid::nil(),
                    "quality",
                    EvaluationStatus::Error,
                    None,
                    false,
                    1.0,
                ),
                finding(
                    Uuid::from_u128(1),
                    "quality",
                    EvaluationStatus::Passed,
                    Some(1.0),
                    false,
                    1.0,
                ),
            ],
        );

        assert_eq!(outcome.aggregate_score, None);
        assert_eq!(outcome.dimension_scores, json!({}));
        assert_eq!(outcome.overall_status, "error");
        assert_eq!(
            outcome.summary["evaluator_completeness"]["required_error_count"],
            1
        );
    }

    #[test]
    fn required_abstention_withholds_scores() {
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
                finding(
                    Uuid::nil(),
                    "quality",
                    EvaluationStatus::Abstained,
                    None,
                    false,
                    1.0,
                ),
                finding(
                    Uuid::from_u128(1),
                    "quality",
                    EvaluationStatus::Passed,
                    Some(0.9),
                    false,
                    1.0,
                ),
            ],
        );

        assert_eq!(outcome.aggregate_score, None);
        assert_eq!(outcome.dimension_scores, json!({}));
        assert_eq!(outcome.overall_status, "error");
        assert_eq!(
            outcome.summary["evaluator_completeness"]["required_abstained_count"],
            1
        );
    }

    #[test]
    fn missing_required_evaluator_withholds_scores() {
        let scored_evaluator_id = Uuid::nil();
        let missing_evaluator_id = Uuid::from_u128(1);
        let bindings = [
            AggregationBinding {
                binding_id: scored_evaluator_id.to_string(),
                required: true,
            },
            AggregationBinding {
                binding_id: missing_evaluator_id.to_string(),
                required: true,
            },
        ];
        let outcome = super::aggregate_results(
            &defaults(),
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                false,
                1.0,
            )]),
            Uuid::nil(),
            &bindings,
            &[finding(
                scored_evaluator_id,
                "quality",
                EvaluationStatus::Passed,
                Some(1.0),
                false,
                1.0,
            )],
        );

        assert_eq!(outcome.aggregate_score, None);
        assert_eq!(outcome.overall_status, "error");
        assert_eq!(
            outcome.summary["evaluator_completeness"]["required_missing_count"],
            1
        );
        assert_eq!(
            outcome.summary["evaluator_completeness"]["incomplete_binding_ids"],
            json!([missing_evaluator_id.to_string()])
        );
    }

    #[test]
    fn optional_diagnostic_error_does_not_affect_score() {
        let required_evaluator_id = Uuid::nil();
        let optional_evaluator_id = Uuid::from_u128(1);
        let bindings = [
            AggregationBinding {
                binding_id: required_evaluator_id.to_string(),
                required: true,
            },
            AggregationBinding {
                binding_id: optional_evaluator_id.to_string(),
                required: false,
            },
        ];
        let outcome = super::aggregate_results(
            &defaults(),
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                false,
                1.0,
            )]),
            Uuid::nil(),
            &bindings,
            &[
                finding(
                    required_evaluator_id,
                    "quality",
                    EvaluationStatus::Passed,
                    Some(1.0),
                    false,
                    1.0,
                ),
                finding(
                    optional_evaluator_id,
                    "diagnostic",
                    EvaluationStatus::Error,
                    None,
                    true,
                    0.0,
                ),
            ],
        );

        assert_eq!(outcome.aggregate_score, Some(1.0));
        assert_eq!(outcome.overall_status, "passed");
        assert_eq!(
            outcome.summary["evaluator_completeness"]["optional_binding_count"],
            1
        );
        assert_eq!(outcome.blocking_failures, json!([]));
    }

    #[test]
    fn score_and_error_for_one_binding_is_rejected_as_duplicate_results() {
        let evaluator_id = Uuid::nil();
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
                finding(
                    evaluator_id,
                    "quality",
                    EvaluationStatus::Passed,
                    Some(1.0),
                    false,
                    1.0,
                ),
                finding(
                    evaluator_id,
                    "quality",
                    EvaluationStatus::Error,
                    None,
                    false,
                    1.0,
                ),
            ],
        );

        assert_eq!(outcome.aggregate_score, None);
        assert_eq!(outcome.overall_status, "error");
        assert_eq!(
            outcome.summary["evaluator_completeness"]["duplicate_result_count"],
            1
        );
    }

    #[test]
    fn all_unscoreable_results_return_error() {
        let outcome = aggregate_findings(
            &defaults(),
            &settings(vec![(
                "quality",
                AggregationMethod::WeightedMean,
                false,
                1.0,
            )]),
            Uuid::nil(),
            &[finding(
                Uuid::nil(),
                "quality",
                EvaluationStatus::Abstained,
                None,
                false,
                1.0,
            )],
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
            &[finding(
                Uuid::nil(),
                "quality",
                EvaluationStatus::Passed,
                Some(0.9),
                false,
                1.0,
            )],
        );

        assert_eq!(outcome.aggregate_score, Some(0.9));
    }

    #[test]
    fn binding_dimension_controls_aggregation_bucket() {
        let record = finding(
            Uuid::nil(),
            "profile_quality",
            EvaluationStatus::Passed,
            Some(0.9),
            false,
            1.0,
        );
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
    fn multiple_results_for_one_binding_withhold_scores() {
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
                finding(
                    Uuid::nil(),
                    "quality",
                    EvaluationStatus::Passed,
                    Some(1.0),
                    false,
                    1.0,
                ),
                finding(
                    Uuid::nil(),
                    "quality",
                    EvaluationStatus::Failed,
                    Some(0.0),
                    false,
                    1.0,
                ),
            ],
        );

        assert_eq!(outcome.aggregate_score, None);
        assert_eq!(outcome.overall_status, "error");
        assert_eq!(
            outcome.summary["evaluator_completeness"]["duplicate_result_count"],
            1
        );
    }
}
