//! Counterbalanced effect estimation and bootstrap confidence intervals.
//!
//! The estimator consumes only complete valid `ABBA`/`BAAB` blocks, computes
//! candidate harm on the log-ratio scale, and reports orientation and position
//! diagnostics. Sampling is fixed in advance; statistics never extend a run
//! until it becomes significant.

use std::collections::BTreeMap;

use anyhow::{
    Result,
    bail,
};
use rand::{
    Rng,
    SeedableRng,
    rngs::StdRng,
};

use super::model::{
    BinaryRole,
    MetricComparison,
    Orientation,
    Sample,
    SampleState,
    Verdict,
};

const BOOTSTRAP_DRAWS: usize = 20_000;

/// Compares baseline and candidate wall time from complete balanced blocks.
///
/// `practical_budget` is a positive harmful-effect threshold. Without one the
/// result remains informative. `confirmation` distinguishes an independently
/// confirmed regression from an initial signal.
pub fn compare_wall_time(
    samples: &[Sample],
    bootstrap_seed: u64,
    practical_budget: Option<f64>,
    max_residual_orientation_effect: Option<f64>,
    confirmation: bool,
) -> Result<MetricComparison> {
    let mut blocks: BTreeMap<u32, Vec<&Sample>> = BTreeMap::new();
    for sample in samples.iter().filter(|sample| sample.measured) {
        blocks.entry(sample.block_id).or_default().push(sample);
    }

    let mut abba = Vec::new();
    let mut baab = Vec::new();
    let mut baseline = Vec::new();
    let mut candidate = Vec::new();
    let mut by_position: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for block in blocks.values_mut() {
        block.sort_by_key(|sample| sample.position);
        if block.len() != 4
            || block
                .iter()
                .any(|sample| sample.validation.state != SampleState::Valid)
        {
            continue;
        }
        let values: Vec<_> = block
            .iter()
            .map(|sample| sample.process.wall_time_ns as f64)
            .collect();
        if values.iter().any(|value| *value <= 0.0) {
            bail!("wall-time samples must be positive");
        }
        for sample in block.iter() {
            match sample.role {
                BinaryRole::Baseline => baseline.push(sample.process.wall_time_ns as f64),
                BinaryRole::Candidate => candidate.push(sample.process.wall_time_ns as f64),
                BinaryRole::Single => bail!("single-binary sample in comparison"),
            }
            by_position
                .entry(format!("{:?}_{}", sample.role, sample.position).to_ascii_lowercase())
                .or_default()
                .push(sample.process.wall_time_ns as f64);
        }
        let effect = match block[0].orientation {
            Orientation::Abba => {
                0.5 * ((values[1] / values[0]).ln() + (values[2] / values[3]).ln())
            }
            Orientation::Baab => {
                0.5 * ((values[0] / values[1]).ln() + (values[3] / values[2]).ln())
            }
            Orientation::Single => bail!("single orientation in comparison"),
        };
        match block[0].orientation {
            Orientation::Abba => abba.push(effect),
            Orientation::Baab => baab.push(effect),
            Orientation::Single => unreachable!(),
        }
    }

    let used = abba.len().min(baab.len());
    let unmatched_blocks = abba.len().abs_diff(baab.len());
    if used == 0 {
        return Ok(empty_comparison(
            practical_budget,
            bootstrap_seed,
            abba.len(),
            baab.len(),
            unmatched_blocks,
        ));
    }
    abba.truncate(used);
    baab.truncate(used);
    let mut effects = Vec::with_capacity(used * 2);
    effects.extend_from_slice(&abba);
    effects.extend_from_slice(&baab);
    let point_log = median(&effects);
    let harmful_effect = point_log.exp() - 1.0;
    let abba_median = median(&abba);
    let baab_median = median(&baab);
    let residual_orientation_effect = (abba_median - baab_median).abs().exp() - 1.0;
    let (confidence_lower, confidence_upper) = bootstrap_interval(&abba, &baab, bootstrap_seed);
    let verdict = decide_verdict(
        confidence_lower,
        confidence_upper,
        practical_budget,
        residual_orientation_effect,
        max_residual_orientation_effect,
        confirmation,
    );

    let baseline_median = median(&baseline);
    let candidate_median = median(&candidate);
    Ok(MetricComparison {
        name: "wall_time".into(),
        unit: "nanoseconds".into(),
        direction: "positive_is_harmful".into(),
        baseline_median,
        candidate_median,
        raw_candidate_delta: candidate_median / baseline_median - 1.0,
        harmful_effect,
        confidence_lower,
        confidence_upper,
        practical_budget,
        verdict,
        valid_abba_blocks: abba.len(),
        valid_baab_blocks: baab.len(),
        unmatched_blocks,
        residual_orientation_effect,
        orientation_medians: BTreeMap::from([
            ("ABBA".into(), abba_median.exp() - 1.0),
            ("BAAB".into(), baab_median.exp() - 1.0),
        ]),
        position_medians: by_position
            .into_iter()
            .map(|(position, values)| (position, median(&values)))
            .collect(),
        estimator: format!("counterbalanced-log-ratio-percentile-bootstrap-v1/{BOOTSTRAP_DRAWS}"),
        bootstrap_seed,
    })
}

fn empty_comparison(
    practical_budget: Option<f64>,
    bootstrap_seed: u64,
    abba: usize,
    baab: usize,
    unmatched: usize,
) -> MetricComparison {
    MetricComparison {
        name: "wall_time".into(),
        unit: "nanoseconds".into(),
        direction: "positive_is_harmful".into(),
        baseline_median: 0.0,
        candidate_median: 0.0,
        raw_candidate_delta: 0.0,
        harmful_effect: 0.0,
        confidence_lower: 0.0,
        confidence_upper: 0.0,
        practical_budget,
        verdict: Verdict::Inconclusive,
        valid_abba_blocks: abba,
        valid_baab_blocks: baab,
        unmatched_blocks: unmatched,
        residual_orientation_effect: 0.0,
        orientation_medians: BTreeMap::new(),
        position_medians: BTreeMap::new(),
        estimator: format!("counterbalanced-log-ratio-percentile-bootstrap-v1/{BOOTSTRAP_DRAWS}"),
        bootstrap_seed,
    }
}

fn bootstrap_interval(abba: &[f64], baab: &[f64], seed: u64) -> (f64, f64) {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut estimates = Vec::with_capacity(BOOTSTRAP_DRAWS);
    let mut draw = Vec::with_capacity(abba.len() + baab.len());
    for _ in 0..BOOTSTRAP_DRAWS {
        draw.clear();
        for _ in 0..abba.len() {
            draw.push(abba[rng.random_range(0..abba.len())]);
        }
        for _ in 0..baab.len() {
            draw.push(baab[rng.random_range(0..baab.len())]);
        }
        estimates.push(median(&draw).exp() - 1.0);
    }
    estimates.sort_by(f64::total_cmp);
    let lower = estimates[(BOOTSTRAP_DRAWS as f64 * 0.025).floor() as usize];
    let upper =
        estimates[((BOOTSTRAP_DRAWS as f64 * 0.975).ceil() as usize).min(BOOTSTRAP_DRAWS - 1)];
    (lower, upper)
}

fn decide_verdict(
    lower: f64,
    upper: f64,
    budget: Option<f64>,
    residual: f64,
    maximum_residual: Option<f64>,
    confirmation: bool,
) -> Verdict {
    if maximum_residual.is_some_and(|maximum| residual > maximum) {
        return Verdict::Invalid;
    }
    let Some(budget) = budget else {
        return Verdict::Informative;
    };
    if upper < 0.0 {
        Verdict::Improvement
    } else if lower > budget {
        if confirmation {
            Verdict::Regression
        } else {
            Verdict::Inconclusive
        }
    } else if upper <= budget {
        Verdict::Pass
    } else {
        Verdict::Inconclusive
    }
}

fn median(values: &[f64]) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    if values.len().is_multiple_of(2) {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
    } else {
        values[values.len() / 2]
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::perf::model::{
        ProcessMeasurement,
        Validation,
    };

    fn samples(candidate_multiplier: f64) -> Vec<Sample> {
        let mut samples = Vec::new();
        for block_id in 0..20 {
            let orientation = if block_id % 2 == 0 {
                Orientation::Abba
            } else {
                Orientation::Baab
            };
            let roles = crate::perf::schedule::executions(orientation);
            for execution in roles {
                let base = 1_000_000.0 + f64::from(block_id * 100 + u32::from(execution.position));
                let duration = if execution.role == BinaryRole::Candidate {
                    base * candidate_multiplier
                } else {
                    base
                };
                samples.push(Sample {
                    schema_id: "sample/v1".into(),
                    run_id: "test".into(),
                    profile_id: "test".into(),
                    workload_id: "workload".into(),
                    tuple_id: "tuple".into(),
                    block_id,
                    orientation_set_id: block_id / 2,
                    orientation,
                    pair_id: execution.pair_id,
                    position: execution.position,
                    role: execution.role,
                    measured: true,
                    started_at: "now".into(),
                    process: ProcessMeasurement {
                        wall_time_ns: duration as u64,
                        cpu_time_ns: None,
                        peak_rss_bytes: None,
                        resource_source: "test".into(),
                        exit_code: Some(0),
                        timed_out: false,
                        stdout_bytes: 0,
                        stderr_bytes: 0,
                        stdout_truncated: false,
                        stderr_truncated: false,
                    },
                    validation: Validation {
                        state: SampleState::Valid,
                        code: "ok".into(),
                        message: "ok".into(),
                    },
                    extra: BTreeMap::new(),
                });
            }
        }
        samples
    }

    #[test]
    fn equivalent_fixture_passes() {
        let comparison =
            compare_wall_time(&samples(1.0), 42, Some(0.05), Some(0.02), true).unwrap();
        assert_eq!(comparison.verdict, Verdict::Pass);
    }

    #[test]
    fn known_slow_fixture_requires_and_honors_confirmation() {
        let unconfirmed =
            compare_wall_time(&samples(1.25), 42, Some(0.05), Some(0.02), false).unwrap();
        assert_eq!(unconfirmed.verdict, Verdict::Inconclusive);
        let confirmed =
            compare_wall_time(&samples(1.25), 42, Some(0.05), Some(0.02), true).unwrap();
        assert_eq!(confirmed.verdict, Verdict::Regression);
    }

    #[test]
    fn residual_orientation_bias_is_invalid() {
        let mut biased = samples(1.0);
        for sample in &mut biased {
            if sample.orientation == Orientation::Abba && sample.role == BinaryRole::Candidate {
                sample.process.wall_time_ns *= 2;
            }
        }
        let comparison = compare_wall_time(&biased, 42, None, Some(0.10), false).unwrap();
        assert_eq!(comparison.verdict, Verdict::Invalid);
    }

    #[test]
    fn unmeasured_readiness_and_preconditioning_are_excluded() {
        let mut measured = samples(1.0);
        let expected = compare_wall_time(&measured, 42, None, None, false)
            .unwrap()
            .harmful_effect;
        let mut discarded = measured[0].clone();
        discarded.measured = false;
        discarded.process.wall_time_ns = u64::MAX;
        measured.push(discarded);
        let actual = compare_wall_time(&measured, 42, None, None, false)
            .unwrap()
            .harmful_effect;
        assert_eq!(actual, expected);
    }
}
