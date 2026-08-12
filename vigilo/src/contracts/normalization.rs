//! Deterministic host-owned measurement normalization.

use crate::contracts::{
    evaluator::Measurement,
    run::{
        NormalizationPolicy,
        NumericMapping,
        ScoreDirection,
        UtilityPoint,
    },
};

const MAX_MAPPING_ENTRIES: usize = 256;
const MAX_LABEL_BYTES: usize = 256;
const MAX_UNIT_BYTES: usize = 64;

fn valid_score(score: f64) -> bool {
    score.is_finite() && (0.0..=1.0).contains(&score)
}

fn validate_domain(min: f64, max: f64) -> Result<(), String> {
    if !min.is_finite() || !max.is_finite() || max <= min {
        return Err("numeric mapping domain must be finite with max greater than min".to_string());
    }
    Ok(())
}

fn validate_points(points: &[UtilityPoint]) -> Result<(), String> {
    if !(2..=MAX_MAPPING_ENTRIES).contains(&points.len()) {
        return Err(format!(
            "piecewise_linear mapping must contain between 2 and {MAX_MAPPING_ENTRIES} points"
        ));
    }
    for (index, point) in points.iter().enumerate() {
        if !point.value.is_finite() || !valid_score(point.score) {
            return Err(format!(
                "piecewise_linear point {index} must have a finite value and score between 0.0 and 1.0"
            ));
        }
        if index > 0 && point.value <= points[index - 1].value {
            return Err("piecewise_linear point values must be strictly increasing".to_string());
        }
    }
    Ok(())
}

/// Validates a normalization policy independently from any evaluator output.
pub(crate) fn validate_policy(policy: &NormalizationPolicy) -> Result<(), String> {
    match policy {
        NormalizationPolicy::Binary {
            false_score,
            true_score,
        } => {
            if !valid_score(*false_score) || !valid_score(*true_score) {
                return Err("binary scores must be finite and between 0.0 and 1.0".to_string());
            }
        }
        NormalizationPolicy::Numeric { unit, mapping } => {
            if let Some(unit) = unit
                && (unit.trim().is_empty() || unit.len() > MAX_UNIT_BYTES)
            {
                return Err(format!(
                    "numeric unit must contain between 1 and {MAX_UNIT_BYTES} bytes"
                ));
            }
            match mapping {
                NumericMapping::Linear { min, max, .. } => validate_domain(*min, *max)?,
                NumericMapping::PiecewiseLinear { points } => validate_points(points)?,
                NumericMapping::Thresholds {
                    min,
                    max,
                    cutpoints,
                    scores,
                } => {
                    validate_domain(*min, *max)?;
                    if cutpoints.len() >= MAX_MAPPING_ENTRIES {
                        return Err(format!(
                            "threshold mapping must contain fewer than {MAX_MAPPING_ENTRIES} cutpoints"
                        ));
                    }
                    if scores.len() != cutpoints.len() + 1 {
                        return Err(
                            "threshold mapping must contain exactly one more score than cutpoints"
                                .to_string(),
                        );
                    }
                    if scores.iter().any(|score| !valid_score(*score)) {
                        return Err(
                            "threshold scores must be finite and between 0.0 and 1.0".to_string()
                        );
                    }
                    for (index, cutpoint) in cutpoints.iter().enumerate() {
                        if !cutpoint.is_finite() || cutpoint <= min || cutpoint >= max {
                            return Err(
                                "threshold cutpoints must be finite and strictly inside the mapping domain"
                                    .to_string(),
                            );
                        }
                        if index > 0 && cutpoint <= &cutpoints[index - 1] {
                            return Err(
                                "threshold cutpoints must be strictly increasing".to_string()
                            );
                        }
                    }
                }
            }
        }
        NormalizationPolicy::Ordinal { values } => {
            if values.is_empty() || values.len() > MAX_MAPPING_ENTRIES {
                return Err(format!(
                    "ordinal mapping must contain between 1 and {MAX_MAPPING_ENTRIES} values"
                ));
            }
            for (label, score) in values {
                if label.trim().is_empty() || label.len() > MAX_LABEL_BYTES {
                    return Err(format!(
                        "ordinal labels must contain between 1 and {MAX_LABEL_BYTES} bytes"
                    ));
                }
                if !valid_score(*score) {
                    return Err("ordinal scores must be finite and between 0.0 and 1.0".to_string());
                }
            }
        }
    }
    Ok(())
}

fn normalize_numeric(mapping: &NumericMapping, value: f64) -> Result<f64, String> {
    if !value.is_finite() {
        return Err("numeric measurement must be finite".to_string());
    }

    match mapping {
        NumericMapping::Linear {
            min,
            max,
            direction,
        } => {
            if value < *min || value > *max {
                return Err("numeric measurement is outside the configured domain".to_string());
            }
            let position = (value - min) / (max - min);
            Ok(match direction {
                ScoreDirection::HigherIsBetter => position,
                ScoreDirection::LowerIsBetter => 1.0 - position,
            })
        }
        NumericMapping::PiecewiseLinear { points } => {
            if value < points[0].value || value > points[points.len() - 1].value {
                return Err("numeric measurement is outside the configured curve".to_string());
            }
            let upper = points.partition_point(|point| point.value < value);
            if upper < points.len() && points[upper].value == value {
                return Ok(points[upper].score);
            }
            let lower = upper - 1;
            let left = &points[lower];
            let right = &points[upper];
            let position = (value - left.value) / (right.value - left.value);
            Ok(left.score + position * (right.score - left.score))
        }
        NumericMapping::Thresholds {
            min,
            max,
            cutpoints,
            scores,
        } => {
            if value < *min || value > *max {
                return Err("numeric measurement is outside the configured domain".to_string());
            }
            let interval = cutpoints.partition_point(|cutpoint| value >= *cutpoint);
            Ok(scores[interval])
        }
    }
}

/// Applies host policy to a raw evaluator measurement without clamping.
pub(crate) fn normalize_measurement(
    policy: &NormalizationPolicy,
    measurement: &Measurement,
) -> Result<f64, String> {
    validate_policy(policy)?;

    let score = match (policy, measurement) {
        (
            NormalizationPolicy::Binary {
                false_score,
                true_score,
            },
            Measurement::Binary { value },
        ) => {
            if *value {
                *true_score
            } else {
                *false_score
            }
        }
        (
            NormalizationPolicy::Numeric {
                unit: expected_unit,
                mapping,
            },
            Measurement::Numeric { value, unit },
        ) => {
            if unit != expected_unit {
                return Err(format!(
                    "numeric measurement unit {:?} does not match configured unit {:?}",
                    unit, expected_unit
                ));
            }
            normalize_numeric(mapping, *value)?
        }
        (NormalizationPolicy::Ordinal { values }, Measurement::Ordinal { value }) => {
            if value.trim().is_empty() || value.len() > MAX_LABEL_BYTES {
                return Err("ordinal measurement label is empty or too long".to_string());
            }
            *values
                .get(value)
                .ok_or_else(|| format!("ordinal measurement value '{value}' is not mapped"))?
        }
        _ => {
            return Err(format!(
                "measurement kind '{}' is incompatible with normalization policy",
                measurement.kind()
            ));
        }
    };

    if !valid_score(score) {
        return Err("normalization produced an invalid score".to_string());
    }
    Ok(score)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn linear_mapping_supports_both_directions() {
        let measurement = Measurement::Numeric {
            value: 25.0,
            unit: Some("ms".to_string()),
        };
        for (direction, expected) in [
            (ScoreDirection::HigherIsBetter, 0.25),
            (ScoreDirection::LowerIsBetter, 0.75),
        ] {
            let policy = NormalizationPolicy::Numeric {
                unit: Some("ms".to_string()),
                mapping: NumericMapping::Linear {
                    min: 0.0,
                    max: 100.0,
                    direction,
                },
            };
            assert_eq!(normalize_measurement(&policy, &measurement), Ok(expected));
        }
    }

    #[test]
    fn piecewise_mapping_interpolates_without_clamping() {
        let policy = NormalizationPolicy::Numeric {
            unit: None,
            mapping: NumericMapping::PiecewiseLinear {
                points: vec![
                    UtilityPoint {
                        value: 0.0,
                        score: 0.0,
                    },
                    UtilityPoint {
                        value: 10.0,
                        score: 1.0,
                    },
                    UtilityPoint {
                        value: 20.0,
                        score: 0.5,
                    },
                ],
            },
        };

        assert_eq!(
            normalize_measurement(
                &policy,
                &Measurement::Numeric {
                    value: 15.0,
                    unit: None,
                }
            ),
            Ok(0.75)
        );
        assert!(
            normalize_measurement(
                &policy,
                &Measurement::Numeric {
                    value: 21.0,
                    unit: None,
                }
            )
            .is_err()
        );
    }

    #[test]
    fn threshold_boundaries_use_the_upper_interval() {
        let policy = NormalizationPolicy::Numeric {
            unit: None,
            mapping: NumericMapping::Thresholds {
                min: 0.0,
                max: 100.0,
                cutpoints: vec![50.0, 80.0],
                scores: vec![0.0, 0.5, 1.0],
            },
        };

        for (value, expected) in [(49.9, 0.0), (50.0, 0.5), (80.0, 1.0), (100.0, 1.0)] {
            assert_eq!(
                normalize_measurement(&policy, &Measurement::Numeric { value, unit: None }),
                Ok(expected)
            );
        }
    }

    #[test]
    fn ordinal_mapping_has_no_universal_tie_value() {
        let policy = NormalizationPolicy::Ordinal {
            values: BTreeMap::from([
                ("preferred".to_string(), 1.0),
                ("tie".to_string(), 0.3),
                ("not_preferred".to_string(), 0.0),
            ]),
        };

        assert_eq!(
            normalize_measurement(
                &policy,
                &Measurement::Ordinal {
                    value: "tie".to_string()
                }
            ),
            Ok(0.3)
        );
    }

    #[test]
    fn invalid_values_units_and_labels_are_rejected() {
        let numeric = NormalizationPolicy::Numeric {
            unit: Some("ms".to_string()),
            mapping: NumericMapping::Linear {
                min: 0.0,
                max: 1.0,
                direction: ScoreDirection::HigherIsBetter,
            },
        };
        assert!(
            normalize_measurement(
                &numeric,
                &Measurement::Numeric {
                    value: f64::NAN,
                    unit: Some("ms".to_string()),
                }
            )
            .is_err()
        );
        assert!(
            normalize_measurement(
                &numeric,
                &Measurement::Numeric {
                    value: 0.5,
                    unit: Some("seconds".to_string()),
                }
            )
            .is_err()
        );

        let ordinal = NormalizationPolicy::Ordinal {
            values: BTreeMap::from([("known".to_string(), 1.0)]),
        };
        assert!(
            normalize_measurement(
                &ordinal,
                &Measurement::Ordinal {
                    value: "unknown".to_string(),
                }
            )
            .is_err()
        );
    }

    #[test]
    fn malformed_policies_are_rejected() {
        let policies = [
            NormalizationPolicy::Binary {
                false_score: f64::NAN,
                true_score: 1.0,
            },
            NormalizationPolicy::Numeric {
                unit: None,
                mapping: NumericMapping::PiecewiseLinear {
                    points: vec![
                        UtilityPoint {
                            value: 1.0,
                            score: 0.0,
                        },
                        UtilityPoint {
                            value: 1.0,
                            score: 1.0,
                        },
                    ],
                },
            },
            NormalizationPolicy::Numeric {
                unit: None,
                mapping: NumericMapping::Thresholds {
                    min: 0.0,
                    max: 1.0,
                    cutpoints: vec![0.5],
                    scores: vec![1.0],
                },
            },
            NormalizationPolicy::Ordinal {
                values: BTreeMap::new(),
            },
        ];

        for policy in policies {
            assert!(validate_policy(&policy).is_err());
        }
    }
}
