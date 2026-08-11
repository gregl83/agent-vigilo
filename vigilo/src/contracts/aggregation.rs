//! Run-level aggregation policy for normalized evaluator findings.
//!
//! This module is intentionally persistence-free. Runtime workers pass
//! profile-resolved bindings and normalized findings in, and receive the exact
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
    pub(crate) evaluator_id: Uuid,
    pub(crate) required: bool,
}

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

fn valid_normalized_score(finding: &AggregationFinding) -> bool {
    scoreable_status(&finding.status)
        && finding
            .normalized_score
            .is_some_and(|score| score.is_finite() && (0.0..=1.0).contains(&score))
}

fn evaluator_completeness(
    bindings: &[AggregationBinding],
    findings: &[AggregationFinding],
) -> (bool, serde_json::Value, BTreeSet<Uuid>) {
    let mut expected = BTreeMap::new();
    let mut duplicate_binding_count = 0usize;
    for binding in bindings {
        if expected
            .insert(binding.evaluator_id, binding.required)
            .is_some()
        {
            duplicate_binding_count += 1;
        }
    }

    let mut findings_by_evaluator = BTreeMap::<Uuid, Vec<&AggregationFinding>>::new();
    for finding in findings {
        findings_by_evaluator
            .entry(finding.evaluator_id)
            .or_default()
            .push(finding);
    }

    let required_evaluator_ids = expected
        .iter()
        .filter_map(|(evaluator_id, required)| required.then_some(*evaluator_id))
        .collect::<BTreeSet<_>>();
    let unexpected_evaluator_ids = findings_by_evaluator
        .keys()
        .filter(|evaluator_id| !expected.contains_key(evaluator_id))
        .copied()
        .collect::<Vec<_>>();
    let mut required_scored_count = 0usize;
    let mut required_error_count = 0usize;
    let mut required_skipped_count = 0usize;
    let mut required_missing_count = 0usize;
    let mut required_unscored_count = 0usize;
    let mut incomplete_evaluator_ids = Vec::new();

    for evaluator_id in &required_evaluator_ids {
        let Some(evaluator_findings) = findings_by_evaluator.get(evaluator_id) else {
            required_missing_count += 1;
            incomplete_evaluator_ids.push(*evaluator_id);
            continue;
        };

        if evaluator_findings
            .iter()
            .any(|finding| finding.status == EvaluationStatus::Error)
        {
            required_error_count += 1;
            incomplete_evaluator_ids.push(*evaluator_id);
        } else if evaluator_findings
            .iter()
            .any(|finding| finding.status == EvaluationStatus::Skipped)
        {
            required_skipped_count += 1;
            incomplete_evaluator_ids.push(*evaluator_id);
        } else if evaluator_findings
            .iter()
            .any(|finding| valid_normalized_score(finding))
        {
            required_scored_count += 1;
        } else {
            required_unscored_count += 1;
            incomplete_evaluator_ids.push(*evaluator_id);
        }
    }

    let complete = !required_evaluator_ids.is_empty()
        && duplicate_binding_count == 0
        && unexpected_evaluator_ids.is_empty()
        && required_scored_count == required_evaluator_ids.len();
    let summary = json!({
        "complete": complete,
        "expected_binding_count": bindings.len(),
        "required_binding_count": required_evaluator_ids.len(),
        "optional_binding_count": expected.len() - required_evaluator_ids.len(),
        "required_scored_count": required_scored_count,
        "required_error_count": required_error_count,
        "required_skipped_count": required_skipped_count,
        "required_missing_count": required_missing_count,
        "required_unscored_count": required_unscored_count,
        "duplicate_binding_count": duplicate_binding_count,
        "unexpected_evaluator_count": unexpected_evaluator_ids.len(),
        "incomplete_evaluator_ids": incomplete_evaluator_ids,
        "unexpected_evaluator_ids": unexpected_evaluator_ids,
    });

    (complete, summary, required_evaluator_ids)
}

fn dimension_score(accumulator: &DimensionAccumulator<'_>) -> Option<f64> {
    let scoreable = accumulator
        .findings
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
    bindings: &[AggregationBinding],
    findings: &[AggregationFinding],
) -> AggregationOutcome {
    let (completeness_is_valid, completeness, required_evaluator_ids) =
        evaluator_completeness(bindings, findings);
    let mut dimensions = BTreeMap::<String, DimensionAccumulator<'_>>::new();
    for (index, finding) in findings.iter().enumerate() {
        if !required_evaluator_ids.contains(&finding.evaluator_id) {
            continue;
        }
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

    let blocking_failures = findings
        .iter()
        .enumerate()
        .filter(|(_, finding)| {
            if !required_evaluator_ids.contains(&finding.evaluator_id) {
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
    let has_scoreable_finding = findings.iter().any(|finding| {
        required_evaluator_ids.contains(&finding.evaluator_id) && valid_normalized_score(finding)
    });

    let overall_status = if !completeness_is_valid {
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

    let result_status_counts = findings
        .iter()
        .fold(BTreeMap::new(), |mut counts, finding| {
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
        findings: &[AggregationFinding],
    ) -> AggregationOutcome {
        let bindings = findings
            .iter()
            .map(|finding| finding.evaluator_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|evaluator_id| AggregationBinding {
                evaluator_id,
                required: true,
            })
            .collect::<Vec<_>>();
        super::aggregate_findings(defaults, settings, attempt_id, &bindings, findings)
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
                finding(
                    id_a,
                    "quality",
                    EvaluationStatus::Passed,
                    Some(1.0),
                    false,
                    1.0,
                ),
                finding(
                    id_a,
                    "quality",
                    EvaluationStatus::Failed,
                    Some(0.0),
                    false,
                    1.0,
                ),
                finding(
                    id_b,
                    "quality",
                    EvaluationStatus::Passed,
                    Some(1.0),
                    false,
                    1.0,
                ),
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
    fn required_skipped_finding_withholds_scores() {
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
                    EvaluationStatus::Skipped,
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
            outcome.summary["evaluator_completeness"]["required_skipped_count"],
            1
        );
    }

    #[test]
    fn missing_required_evaluator_withholds_scores() {
        let scored_evaluator_id = Uuid::nil();
        let missing_evaluator_id = Uuid::from_u128(1);
        let bindings = [
            AggregationBinding {
                evaluator_id: scored_evaluator_id,
                required: true,
            },
            AggregationBinding {
                evaluator_id: missing_evaluator_id,
                required: true,
            },
        ];
        let outcome = super::aggregate_findings(
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
            outcome.summary["evaluator_completeness"]["incomplete_evaluator_ids"],
            json!([missing_evaluator_id])
        );
    }

    #[test]
    fn optional_diagnostic_error_does_not_affect_score() {
        let required_evaluator_id = Uuid::nil();
        let optional_evaluator_id = Uuid::from_u128(1);
        let bindings = [
            AggregationBinding {
                evaluator_id: required_evaluator_id,
                required: true,
            },
            AggregationBinding {
                evaluator_id: optional_evaluator_id,
                required: false,
            },
        ];
        let outcome = super::aggregate_findings(
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
    fn score_and_error_from_one_required_evaluator_is_incomplete() {
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
            outcome.summary["evaluator_completeness"]["required_error_count"],
            1
        );
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
            &[finding(
                Uuid::nil(),
                "quality",
                EvaluationStatus::Skipped,
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
            &settings(vec![(
                "profile_safety",
                AggregationMethod::MinScore,
                true,
                1.0,
            )]),
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

        assert_eq!(outcome.summary["result_count"], 2);
        assert_eq!(outcome.summary["result_status_counts"]["passed"], 1);
        assert_eq!(outcome.summary["result_status_counts"]["failed"], 1);
    }
}
