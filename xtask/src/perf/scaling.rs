//! Acceptance analysis for component scaling models.
//!
//! Continuous models fit a fixed intercept and per-unit slope to every raw
//! observation. Stepped models estimate each declared cardinality separately
//! so batching discontinuities are never smoothed away. Both forms require
//! repeated valid samples, exact external amplification, and bounded residuals.

use std::{
    collections::BTreeMap,
    fs,
    path::{
        Path,
        PathBuf,
    },
};

use anyhow::{
    Context,
    Result,
    bail,
};
use clap::Args;
use serde::Serialize;

use super::{
    EXIT_INVALID,
    EXIT_PASS,
    artifact::{
        atomic_json,
        require_artifact_path,
        workspace_root,
    },
    config::load_registry,
    model::{
        Sample,
        SampleState,
        ScalingKind,
        ScalingModel,
        Workload,
    },
};

const MODEL_SCHEMA: &str = "component-models/v1";

/// Arguments for fitting registered component models from a completed run.
#[derive(Debug, Args)]
pub struct ModelArgs {
    /// Completed run directory containing `samples.jsonl`.
    #[arg(long)]
    run_dir: PathBuf,
}

#[derive(Debug, Serialize)]
struct ModelDocument {
    schema_id: &'static str,
    source_run: String,
    models: Vec<ModelResult>,
}

#[derive(Debug, Serialize)]
struct ModelResult {
    workload_id: String,
    kind: String,
    accepted: bool,
    sample_count: usize,
    fixed_ns: Option<f64>,
    slope_ns_per_unit: Option<f64>,
    step_ns: BTreeMap<u64, f64>,
    max_residual_fraction: Option<f64>,
    message: String,
}

/// Fits every registered scaling workload present in a run and writes owned JSON.
pub fn execute(args: ModelArgs) -> Result<u8> {
    let root = workspace_root()?;
    let run_dir = require_artifact_path(&root, &args.run_dir)?;
    let samples = read_samples(&run_dir.join("samples.jsonl"))?;
    let registry = load_registry(&root)?;
    let mut models = Vec::new();
    for workload in registry
        .workloads
        .iter()
        .filter(|workload| workload.scaling_model.is_some())
    {
        let selected = samples
            .iter()
            .filter(|sample| sample.workload_id == workload.id)
            .collect::<Vec<_>>();
        if !selected.is_empty() {
            models.push(analyze(workload, &selected));
        }
    }
    if models.is_empty() {
        bail!("run contains no registered component scaling samples");
    }
    let accepted = models.iter().all(|model| model.accepted);
    let document = ModelDocument {
        schema_id: MODEL_SCHEMA,
        source_run: run_dir.display().to_string(),
        models,
    };
    let output = run_dir.join("component-models.json");
    atomic_json(&output, &document)?;
    println!("Component models: {}", output.display());
    Ok(if accepted { EXIT_PASS } else { EXIT_INVALID })
}

fn read_samples(path: &Path) -> Result<Vec<Sample>> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("parse {} line {}", path.display(), index + 1))
        })
        .collect()
}

fn analyze(workload: &Workload, samples: &[&Sample]) -> ModelResult {
    let model = workload
        .scaling_model
        .as_ref()
        .expect("caller selected a scaling workload");
    match analyze_result(workload, model, samples) {
        Ok(fit) => fit,
        Err(error) => ModelResult {
            workload_id: workload.id.clone(),
            kind: kind_name(&model.kind).into(),
            accepted: false,
            sample_count: samples.len(),
            fixed_ns: None,
            slope_ns_per_unit: None,
            step_ns: BTreeMap::new(),
            max_residual_fraction: None,
            message: format!("{error:#}"),
        },
    }
}

fn analyze_result(
    workload: &Workload,
    model: &ScalingModel,
    samples: &[&Sample],
) -> Result<ModelResult> {
    let points = model
        .points
        .iter()
        .map(|point| (point.tuple.as_str(), point))
        .collect::<BTreeMap<_, _>>();
    let mut observations = Vec::with_capacity(samples.len());
    let mut repetitions = BTreeMap::<&str, usize>::new();
    for sample in samples {
        if !sample.measured || sample.validation.state != SampleState::Valid {
            bail!("model input contains an unmeasured or invalid sample");
        }
        let point = points
            .get(sample.tuple_id.as_str())
            .with_context(|| format!("unregistered model tuple {}", sample.tuple_id))?;
        verify_exact(sample, &point.exact)?;
        *repetitions.entry(&sample.tuple_id).or_default() += 1;
        observations.push((point.input as f64, sample.process.wall_time_ns as f64));
    }
    if repetitions.len() < model.points.len()
        || repetitions.values().any(|repetitions| *repetitions < 2)
    {
        bail!("every model point requires at least two measured samples");
    }

    let (fixed, slope, steps, predictions): (_, _, _, Vec<f64>) = match model.kind {
        ScalingKind::FixedPlusSlope => {
            let (fixed, slope) = least_squares(&observations)?;
            let predictions = observations
                .iter()
                .map(|(input, _)| fixed + slope * input)
                .collect();
            (Some(fixed), Some(slope), BTreeMap::new(), predictions)
        }
        ScalingKind::Stepped => {
            let mut grouped = BTreeMap::<u64, Vec<f64>>::new();
            for (input, elapsed) in &observations {
                grouped.entry(*input as u64).or_default().push(*elapsed);
            }
            let steps = grouped
                .into_iter()
                .map(|(input, values)| (input, median(values)))
                .collect::<BTreeMap<_, _>>();
            let predictions = observations
                .iter()
                .map(|(input, _)| steps[&(*input as u64)])
                .collect();
            (None, None, steps, predictions)
        }
    };
    let maximum = observations
        .iter()
        .zip(&predictions)
        .map(|((_, observed), predicted)| relative_residual(*observed, *predicted))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .fold(0.0, f64::max);
    if maximum > model.max_residual_fraction {
        bail!(
            "maximum residual {:.4} exceeds registered limit {:.4}",
            maximum,
            model.max_residual_fraction
        );
    }
    Ok(ModelResult {
        workload_id: workload.id.clone(),
        kind: kind_name(&model.kind).into(),
        accepted: true,
        sample_count: samples.len(),
        fixed_ns: fixed,
        slope_ns_per_unit: slope,
        step_ns: steps,
        max_residual_fraction: Some(maximum),
        message: "repeated samples, exact observations, and residual limit passed".into(),
    })
}

fn least_squares(observations: &[(f64, f64)]) -> Result<(f64, f64)> {
    if observations.len() < 3 {
        bail!("fixed-plus-slope fit requires at least three observations");
    }
    let count = observations.len() as f64;
    let sum_x = observations.iter().map(|(x, _)| x).sum::<f64>();
    let sum_y = observations.iter().map(|(_, y)| y).sum::<f64>();
    let sum_xx = observations.iter().map(|(x, _)| x * x).sum::<f64>();
    let sum_xy = observations.iter().map(|(x, y)| x * y).sum::<f64>();
    let denominator = count * sum_xx - sum_x * sum_x;
    if denominator.abs() <= f64::EPSILON {
        bail!("fixed-plus-slope inputs do not identify a slope");
    }
    let slope = (count * sum_xy - sum_x * sum_y) / denominator;
    let fixed = (sum_y - slope * sum_x) / count;
    if !fixed.is_finite() || !slope.is_finite() || fixed < 0.0 || slope < 0.0 {
        bail!("fixed-plus-slope coefficients must be finite and nonnegative");
    }
    Ok((fixed, slope))
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

fn relative_residual(observed: f64, predicted: f64) -> Result<f64> {
    if !(observed.is_finite() && predicted.is_finite() && predicted > 0.0) {
        bail!("model observations and predictions must be finite and positive");
    }
    Ok((observed - predicted).abs() / predicted)
}

fn verify_exact(sample: &Sample, expected: &BTreeMap<String, u64>) -> Result<()> {
    for (metric, expected) in expected {
        let actual = match metric.as_str() {
            "http_requests" => sample.external.http_requests,
            "queue_ready" => sample.external.queue_ready,
            "queue_unacked" => sample.external.queue_unacked,
            metric if metric.starts_with("durable.") => sample
                .external
                .durable_counts
                .get(metric.trim_start_matches("durable."))
                .and_then(|value| u64::try_from(*value).ok()),
            _ => bail!("unsupported exact observation {metric}"),
        };
        if actual != Some(*expected) {
            bail!("exact observation {metric} differs in model input");
        }
    }
    Ok(())
}

fn kind_name(kind: &ScalingKind) -> &'static str {
    match kind {
        ScalingKind::FixedPlusSlope => "fixed_plus_slope",
        ScalingKind::Stepped => "stepped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::model::{
        BinaryRole,
        ExternalMeasurements,
        Orientation,
        ProcessMeasurement,
        SAMPLE_SCHEMA,
        Validation,
    };

    fn workload(id: &str) -> Workload {
        let root = workspace_root().unwrap();
        load_registry(&root)
            .unwrap()
            .workloads
            .into_iter()
            .find(|workload| workload.id == id)
            .unwrap()
    }

    fn samples_for(workload: &Workload) -> Vec<Sample> {
        workload
            .scaling_model
            .as_ref()
            .unwrap()
            .points
            .iter()
            .flat_map(|point| {
                (0..2).map(move |repetition| {
                    let mut external = ExternalMeasurements::default();
                    for (metric, value) in &point.exact {
                        match metric.as_str() {
                            "http_requests" => external.http_requests = Some(*value),
                            "queue_ready" => external.queue_ready = Some(*value),
                            "queue_unacked" => external.queue_unacked = Some(*value),
                            metric if metric.starts_with("durable.") => {
                                external.durable_counts.insert(
                                    metric.trim_start_matches("durable.").into(),
                                    *value as i64,
                                );
                            }
                            _ => unreachable!(),
                        }
                    }
                    Sample {
                        schema_id: SAMPLE_SCHEMA.into(),
                        run_id: "run".into(),
                        profile_id: "profile".into(),
                        workload_id: workload.id.clone(),
                        tuple_id: point.tuple.clone(),
                        block_id: repetition,
                        orientation_set_id: repetition,
                        orientation: Orientation::Single,
                        pair_id: repetition as u8,
                        position: 1,
                        role: BinaryRole::Single,
                        measured: true,
                        started_at: "now".into(),
                        process: ProcessMeasurement {
                            wall_time_ns: 1_000 + point.input * 10,
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
                        external,
                        extra: BTreeMap::new(),
                    }
                })
            })
            .collect()
    }

    #[test]
    fn linear_fit_accepts_fixed_cost_and_slope() {
        let observations = [(1.0, 12.0), (2.0, 14.0), (4.0, 18.0)];
        let (fixed, slope) = least_squares(&observations).unwrap();
        assert!((fixed - 10.0).abs() < 1e-9);
        assert!((slope - 2.0).abs() < 1e-9);
    }

    #[test]
    fn linear_fit_and_residuals_reject_invalid_evidence() {
        assert!(least_squares(&[(1.0, 1.0), (1.0, 2.0), (1.0, 3.0)]).is_err());
        assert!(least_squares(&[(1.0, 4.0), (2.0, 3.0), (3.0, 2.0)]).is_err());
        assert!(relative_residual(1.0, 0.0).is_err());
        assert_eq!(median(vec![1.0, 4.0, 2.0, 3.0]), 2.5);
    }

    #[test]
    fn registered_fixed_and_stepped_models_accept_complete_exact_samples() {
        for id in ["run.create-scaling.v1", "coordinator.outbox-scaling.v1"] {
            let workload = workload(id);
            let samples = samples_for(&workload);
            let references = samples.iter().collect::<Vec<_>>();
            let result = analyze(&workload, &references);
            assert!(result.accepted, "{}", result.message);
            assert_eq!(result.sample_count, samples.len());
        }
    }

    #[test]
    fn model_analysis_rejects_oracle_drift_and_missing_repetition() {
        let workload = workload("run.create-scaling.v1");
        let mut samples = samples_for(&workload);
        samples[0].external.durable_counts.insert("runs".into(), 2);
        let references = samples.iter().collect::<Vec<_>>();
        assert!(!analyze(&workload, &references).accepted);

        let samples = samples_for(&workload)
            .into_iter()
            .enumerate()
            .filter_map(|(index, sample)| (index != 0).then_some(sample))
            .collect::<Vec<_>>();
        let references = samples.iter().collect::<Vec<_>>();
        assert!(!analyze(&workload, &references).accepted);
    }
}
