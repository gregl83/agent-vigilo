//! Canonical noise, bounded-capacity, and baseline calibration.
//!
//! Calibration keeps three concerns separate: no-change comparisons establish
//! repeatability and sample counts, capacity runs locate a bounded one/two-worker
//! knee, and reviewed publication turns evidence into versioned budget/profile
//! artifacts. No local or noisy-host result may silently become a blocking gate.

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
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
use chrono::Utc;
use clap::{
    Args,
    Subcommand,
};
use serde::{
    Deserialize,
    de::DeserializeOwned,
};

use super::{
    EXIT_INVALID,
    EXIT_PASS,
    artifact::{
        atomic_json,
        atomic_text,
        digest_file,
        require_artifact_subpath,
        workspace_root,
    },
    model::{
        BASELINE_SCHEMA,
        BUDGET_SCHEMA,
        BUILD_SCHEMA,
        BaselineDocument,
        BudgetEntry,
        BudgetPolicy,
        BuildManifest,
        CALIBRATION_SCHEMA,
        CAPACITY_SCHEMA,
        CalibrationDocument,
        CalibrationMetric,
        CalibrationTarget,
        CampaignManifest,
        CapacityDocument,
        CapacityKnee,
        CapacityPoint,
        ENVIRONMENT_SCHEMA,
        EnvironmentManifest,
        MetricComparison,
        PROFILE_SCHEMA,
        Profile,
        ProfileWorkload,
        REPORT_SCHEMA,
        ReportDocument,
        SAMPLE_SCHEMA,
        Sample,
        SampleState,
    },
    provenance,
    workload::capacity_tuple,
};

const TARGETS_SCHEMA: &str = "calibration-targets/v1";

/// Arguments for calibration evidence analysis.
#[derive(Debug, Args)]
pub struct CalibrateArgs {
    #[command(subcommand)]
    command: CalibrateCommand,
}

#[derive(Debug, Subcommand)]
enum CalibrateCommand {
    /// Check whether a canonical host is stable enough to resolve regression budgets.
    Noise(NoiseArgs),
    /// Find bounded one- and two-worker capacity without comparing revisions.
    Capacity(CapacityArgs),
    /// Freeze reviewed calibration evidence into a gating profile and budget policy.
    Publish(PublishArgs),
}

#[derive(Debug, Args)]
struct NoiseArgs {
    /// Completed `calibration-v1` comparison directory.
    #[arg(long)]
    run_dir: PathBuf,
    /// Reviewed provisional targets; defaults to the repository calibration targets.
    #[arg(long)]
    targets: Option<PathBuf>,
    /// Output file; defaults to `<run-dir>/calibration.json`.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct CapacityArgs {
    /// Completed `capacity-v1` single-binary run directory.
    #[arg(long)]
    run_dir: PathBuf,
    /// Reviewed capacity policy; defaults to the repository calibration targets.
    #[arg(long)]
    targets: Option<PathBuf>,
    /// Output file; defaults to `<run-dir>/capacity.json`.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
struct PublishArgs {
    /// Publishable canonical noise evidence.
    #[arg(long)]
    calibration: PathBuf,
    /// Matching bounded capacity evidence.
    #[arg(long)]
    capacity: PathBuf,
    /// Immutable build manifest shared by the evidence.
    #[arg(long = "build-manifest")]
    build_manifest: PathBuf,
    /// Versioned profile identity, normally `reference-v2`.
    #[arg(long, default_value = "reference-v2")]
    id: String,
    /// Human or review process approving the practical budgets.
    #[arg(long = "approved-by")]
    approved_by: String,
    /// New immutable output directory under `target/perf/baselines`.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct CalibrationTargets {
    schema_id: String,
    id: String,
    metrics: Vec<CalibrationTarget>,
    capacity: CapacityPolicy,
}

#[derive(Debug, Deserialize)]
struct CapacityPolicy {
    minimum_throughput_gain: f64,
    latency_growth: f64,
    maximum_worker_cpu_percent: f64,
    maximum_service_cpu_percent: f64,
    minimum_samples_per_point: usize,
    minimum_scale_efficiency: f64,
}

/// Executes one offline calibration analysis command.
pub fn execute(args: CalibrateArgs) -> Result<u8> {
    let root = workspace_root()?;
    match args.command {
        CalibrateCommand::Noise(args) => analyze_noise_run(&root, args),
        CalibrateCommand::Capacity(args) => analyze_capacity_run(&root, args),
        CalibrateCommand::Publish(args) => publish_baseline(&root, args),
    }
}

/// Validates checked-in calibration targets without provisioning services.
pub(super) fn validate_repository_contract(root: &Path) -> Result<()> {
    let targets = load_targets(root, None)?;
    validate_capacity_policy(&targets.capacity)?;
    let mut names = BTreeSet::new();
    for target in &targets.metrics {
        if target.metric.is_empty()
            || !(target.practical_budget.is_finite() && target.practical_budget > 0.0)
            || !(target.target_power.is_finite()
                && target.target_power > 0.5
                && target.target_power < 1.0)
            || !(target.max_residual_orientation_effect.is_finite()
                && target.max_residual_orientation_effect > 0.0)
        {
            bail!("calibration targets contain an invalid metric contract");
        }
        if !names.insert(&target.metric) {
            bail!("calibration targets contain a duplicate metric");
        }
    }
    Ok(())
}

/// Validates and summarizes a completed no-change comparison.
fn analyze_noise_run(root: &Path, args: NoiseArgs) -> Result<u8> {
    let report: ReportDocument = read_json(&args.run_dir.join("report.json"))?;
    let campaign: CampaignManifest = read_json(&args.run_dir.join("campaign.json"))?;
    let environment: EnvironmentManifest = read_json(&args.run_dir.join("environment.json"))?;
    validate_source_documents(
        &report,
        &campaign,
        &environment,
        "compare",
        "calibration-v1",
    )?;
    let frozen = provenance::load(&args.run_dir, &campaign)
        .context("load noise-calibration campaign provenance")?;
    let profile = &frozen.profile;
    if profile
        .workloads
        .iter()
        .any(|workload| workload.timing != "calibration" || workload.blocks < 30)
    {
        bail!("noise calibration requires at least 30 blocks for every selected tuple");
    }
    if report.comparisons.len() != campaign.selected_workloads.len() {
        bail!("noise calibration is missing a selected workload comparison");
    }
    let baseline_manifest = frozen
        .baseline_manifest
        .as_ref()
        .context("noise calibration omitted its frozen baseline manifest")?;
    if baseline_manifest.executable_digest != frozen.candidate_manifest.executable_digest {
        bail!("noise calibration must use one identical frozen build");
    }
    let targets = load_targets(root, args.targets.as_deref())?;
    let mut build_digest = None;
    let mut metrics = Vec::new();
    let mut failures = Vec::new();
    if !environment.canonical {
        failures.push("noncanonical calibration evidence cannot publish budgets".into());
    }
    for comparison in &report.comparisons {
        if comparison.baseline_digest != comparison.candidate_digest {
            bail!("noise calibration must compare one identical immutable build");
        }
        if comparison.baseline_digest != baseline_manifest.executable_digest {
            bail!("noise calibration comparison digest differs from frozen build provenance");
        }
        match &build_digest {
            Some(digest) if digest != &comparison.baseline_digest => {
                bail!("noise calibration mixed build digests")
            }
            None => build_digest = Some(comparison.baseline_digest.clone()),
            _ => {}
        }
        for target in &targets.metrics {
            let metric = comparison
                .metrics
                .iter()
                .find(|metric| metric.name == target.metric)
                .with_context(|| {
                    format!(
                        "{}:{} has no {} calibration metric",
                        comparison.workload_id, comparison.tuple_id, target.metric
                    )
                })?;
            let evidence = calibrate_metric(
                &comparison.workload_id,
                &comparison.tuple_id,
                metric,
                target,
            )?;
            if !evidence.repeatable {
                failures.push(format!(
                    "{}:{} {} noise {:.2}% cannot support {:.2}% budget with {} blocks",
                    comparison.workload_id,
                    comparison.tuple_id,
                    metric.name,
                    evidence.noise_bound * 100.0,
                    target.practical_budget * 100.0,
                    evidence.available_blocks
                ));
            }
            metrics.push(evidence);
        }
    }
    if metrics.is_empty() {
        bail!("noise calibration produced no metric evidence");
    }
    let document = CalibrationDocument {
        schema_id: CALIBRATION_SCHEMA.into(),
        id: format!("{}-{}", targets.id, report.run_id),
        created_at: Utc::now().to_rfc3339(),
        source_run_id: report.run_id,
        environment_id: environment.environment_id,
        build_digest: build_digest.context("noise calibration has no comparisons")?,
        publishable: failures.is_empty(),
        metrics,
        failures,
    };
    let output = args
        .output
        .unwrap_or_else(|| args.run_dir.join("calibration.json"));
    atomic_json(&output, &document)?;
    println!(
        "Noise calibration: {}",
        if document.publishable {
            "PASS"
        } else {
            "INVALID"
        }
    );
    println!("Evidence: {}", output.display());
    Ok(if document.publishable {
        EXIT_PASS
    } else {
        EXIT_INVALID
    })
}

/// Reduces raw staircase samples into reproducible capacity points and knees.
fn analyze_capacity_run(root: &Path, args: CapacityArgs) -> Result<u8> {
    let report: ReportDocument = read_json(&args.run_dir.join("report.json"))?;
    let campaign: CampaignManifest = read_json(&args.run_dir.join("campaign.json"))?;
    let environment: EnvironmentManifest = read_json(&args.run_dir.join("environment.json"))?;
    validate_source_documents(&report, &campaign, &environment, "run", "capacity-v1")?;
    let frozen =
        provenance::load(&args.run_dir, &campaign).context("load capacity campaign provenance")?;
    let profile = &frozen.profile;
    if profile
        .workloads
        .iter()
        .any(|workload| workload.timing != "capacity")
    {
        bail!("capacity calibration profile mixes fixed-load and capacity work");
    }
    let targets = load_targets(root, args.targets.as_deref())?;
    validate_capacity_policy(&targets.capacity)?;
    let samples: Vec<Sample> = read_jsonl(&args.run_dir.join("samples.jsonl"))?;
    let mut grouped: BTreeMap<String, Vec<&Sample>> = BTreeMap::new();
    for sample in samples.iter().filter(|sample| sample.measured) {
        if sample.schema_id != SAMPLE_SCHEMA || sample.validation.state != SampleState::Valid {
            bail!("capacity evidence contains an invalid measured sample");
        }
        grouped
            .entry(sample.tuple_id.clone())
            .or_default()
            .push(sample);
    }

    let mut points = Vec::new();
    for selected in &profile.workloads {
        let samples = grouped
            .get(&selected.tuple)
            .with_context(|| format!("capacity tuple {} has no samples", selected.tuple))?;
        if samples.len() < targets.capacity.minimum_samples_per_point {
            bail!(
                "capacity tuple {} has {} samples; {} required",
                selected.tuple,
                samples.len(),
                targets.capacity.minimum_samples_per_point
            );
        }
        let (workers, load_step) = capacity_tuple(&selected.tuple)?;
        let cases = samples[0]
            .external
            .durable_counts
            .get("executions")
            .copied()
            .context("capacity sample has no exact executions count")?;
        let cases = u64::try_from(cases)?;
        let mut throughputs = Vec::new();
        let mut latencies = Vec::new();
        let mut process_cpu = Vec::new();
        let mut service_cpu = Vec::new();
        for sample in samples {
            if sample.external.durable_counts.get("executions").copied() != Some(cases as i64) {
                bail!(
                    "capacity tuple {} has inconsistent case counts",
                    selected.tuple
                );
            }
            if sample.process.wall_time_ns == 0 {
                bail!("capacity tuple {} has zero wall time", selected.tuple);
            }
            throughputs.push(cases as f64 * 1_000_000_000.0 / sample.process.wall_time_ns as f64);
            latencies.push(sample.process.wall_time_ns as f64 / 1_000_000.0);
            let cpu_time = sample
                .process
                .cpu_time_ns
                .context("capacity sample is missing process CPU")?;
            process_cpu.push(
                cpu_time as f64 * 100.0 / sample.process.wall_time_ns as f64 / workers as f64,
            );
            service_cpu.push(
                sample
                    .external
                    .service_cpu_percent
                    .context("capacity sample is missing shared-service CPU")?,
            );
        }
        points.push(CapacityPoint {
            workers: u32::try_from(workers)?,
            load_step: u32::try_from(load_step)?,
            cases,
            samples: samples.len(),
            throughput_per_second: median(&throughputs),
            p95_latency_ms: percentile(&latencies, 0.95),
            process_cpu_percent_per_worker: process_cpu
                .into_iter()
                .max_by(f64::total_cmp)
                .unwrap_or_default(),
            service_cpu_percent: service_cpu.into_iter().max_by(f64::total_cmp),
        });
    }

    points.sort_by_key(|point| (point.workers, point.load_step));
    let one = find_knee(
        1,
        &points,
        targets.capacity.minimum_throughput_gain,
        targets.capacity.latency_growth,
        targets.capacity.maximum_worker_cpu_percent,
        targets.capacity.maximum_service_cpu_percent,
    )?;
    let two = find_knee(
        2,
        &points,
        targets.capacity.minimum_throughput_gain,
        targets.capacity.latency_growth,
        targets.capacity.maximum_worker_cpu_percent,
        targets.capacity.maximum_service_cpu_percent,
    )?;
    let scale_efficiency_2 = one
        .knee_throughput_per_second
        .zip(two.knee_throughput_per_second)
        .map(|(one, two)| (two / (2.0 * one)).min(1.0));
    let supports_linear_projection = scale_efficiency_2
        .is_some_and(|efficiency| efficiency >= targets.capacity.minimum_scale_efficiency);
    let document = CapacityDocument {
        schema_id: CAPACITY_SCHEMA.into(),
        source_run_id: report.run_id,
        build_digest: frozen.candidate_manifest.executable_digest,
        environment_id: environment.environment_id,
        canonical: environment.canonical,
        points,
        knees: vec![one, two],
        scale_efficiency_2,
        supports_linear_projection,
        failures: Vec::new(),
    };
    let output = args
        .output
        .unwrap_or_else(|| args.run_dir.join("capacity.json"));
    atomic_json(&output, &document)?;
    println!(
        "Capacity calibration: {}",
        if document.canonical {
            "PASS"
        } else {
            "INFORMATIVE"
        }
    );
    println!("Evidence: {}", output.display());
    Ok(EXIT_PASS)
}

/// Freezes matching reviewed evidence without overwriting an earlier baseline.
fn publish_baseline(root: &Path, args: PublishArgs) -> Result<u8> {
    let calibration: CalibrationDocument = read_json(&args.calibration)?;
    let capacity: CapacityDocument = read_json(&args.capacity)?;
    let build_manifest: BuildManifest = read_json(&args.build_manifest)?;
    if calibration.schema_id != CALIBRATION_SCHEMA
        || !calibration.publishable
        || !calibration.failures.is_empty()
    {
        bail!("baseline publication requires publishable canonical noise evidence");
    }
    if capacity.schema_id != CAPACITY_SCHEMA
        || !capacity.canonical
        || capacity.environment_id != calibration.environment_id
        || !capacity.failures.is_empty()
    {
        bail!("baseline publication requires valid bounded-capacity evidence");
    }
    let targets = load_targets(root, None)?;
    validate_capacity_evidence(&capacity, &targets.capacity)?;
    if build_manifest.schema_id != BUILD_SCHEMA
        || calibration.build_digest != build_manifest.executable_digest
        || capacity.build_digest != build_manifest.executable_digest
    {
        bail!("calibration, capacity, and build-manifest digests do not match");
    }
    let policy = build_budget_policy(
        &args.id,
        &args.approved_by,
        &calibration.environment_id,
        &calibration.id,
        &calibration.metrics,
    )?;
    let profile = build_reference_profile(&args.id, &policy, &calibration.metrics)?;
    let output = args
        .output
        .clone()
        .unwrap_or_else(|| root.join("target/perf/baselines").join(&args.id));
    let output = require_artifact_subpath(root, &output, "baselines")?;
    if output.exists() {
        bail!("calibrated baseline already exists: {}", output.display());
    }
    let parent = output.parent().context("baseline output has no parent")?;
    fs::create_dir_all(parent)?;
    let name = output.file_name().context("baseline output has no name")?;
    let staged = parent.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    fs::create_dir(&staged).with_context(|| format!("create {}", staged.display()))?;
    let result = write_baseline_files(
        &staged,
        &args,
        &calibration,
        &build_manifest,
        &policy,
        &profile,
    );
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staged);
        return Err(error);
    }
    fs::rename(&staged, &output).with_context(|| format!("commit {}", output.display()))?;
    println!("Published calibrated baseline: {}", output.display());
    Ok(EXIT_PASS)
}

/// Recomputes the exact capacity staircase before evidence can become a baseline.
fn validate_capacity_evidence(document: &CapacityDocument, policy: &CapacityPolicy) -> Result<()> {
    validate_capacity_policy(policy)?;
    let expected = [1, 2]
        .into_iter()
        .flat_map(|workers| {
            [1, 2, 4, 8, 16]
                .into_iter()
                .map(move |load| (workers, load))
        })
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    for point in &document.points {
        if point.samples < policy.minimum_samples_per_point
            || !expected.contains(&(point.workers, point.load_step))
            || !observed.insert((point.workers, point.load_step))
        {
            bail!("capacity publication requires every exact staircase point");
        }
    }
    if observed != expected {
        bail!("capacity publication requires every exact staircase point");
    }
    let recomputed = [1, 2]
        .into_iter()
        .map(|workers| {
            find_knee(
                workers,
                &document.points,
                policy.minimum_throughput_gain,
                policy.latency_growth,
                policy.maximum_worker_cpu_percent,
                policy.maximum_service_cpu_percent,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    if document.knees.len() != recomputed.len()
        || document
            .knees
            .iter()
            .zip(&recomputed)
            .any(|(recorded, recomputed)| !same_knee(recorded, recomputed))
    {
        bail!("capacity publication knee evidence does not recompute");
    }
    let efficiency = recomputed[0]
        .knee_throughput_per_second
        .zip(recomputed[1].knee_throughput_per_second)
        .map(|(one, two)| (two / (2.0 * one)).min(1.0));
    if !same_optional_float(document.scale_efficiency_2, efficiency)
        || document.supports_linear_projection
            != efficiency.is_some_and(|value| value >= policy.minimum_scale_efficiency)
    {
        bail!("capacity publication scale-efficiency evidence does not recompute");
    }
    Ok(())
}

fn same_knee(left: &CapacityKnee, right: &CapacityKnee) -> bool {
    left.workers == right.workers
        && left.knee_step == right.knee_step
        && same_optional_float(
            left.knee_throughput_per_second,
            right.knee_throughput_per_second,
        )
        && same_optional_float(
            left.observed_rate_lower_bound,
            right.observed_rate_lower_bound,
        )
}

fn same_optional_float(left: Option<f64>, right: Option<f64>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => (left - right).abs() <= f64::EPSILON,
        (None, None) => true,
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn write_baseline_files(
    output: &Path,
    args: &PublishArgs,
    calibration: &CalibrationDocument,
    build_manifest: &BuildManifest,
    policy: &BudgetPolicy,
    profile: &Profile,
) -> Result<()> {
    let calibration_file = output.join("calibration.json");
    let capacity_file = output.join("capacity.json");
    let budget_relative = format!("budgets/{}.toml", policy.id);
    let profile_relative = format!("profiles/{}.toml", profile.id);
    let budget_file = output.join(&budget_relative);
    let profile_file = output.join(&profile_relative);
    let build_file = output.join("build-manifest.json");
    fs::copy(&args.calibration, &calibration_file)?;
    fs::copy(&args.capacity, &capacity_file)?;
    atomic_text(&budget_file, &toml::to_string_pretty(policy)?)?;
    atomic_text(&profile_file, &toml::to_string_pretty(profile)?)?;
    atomic_json(&build_file, build_manifest)?;
    let baseline = BaselineDocument {
        schema_id: BASELINE_SCHEMA.into(),
        id: profile.id.clone(),
        created_at: Utc::now().to_rfc3339(),
        environment_id: calibration.environment_id.clone(),
        build_digest: build_manifest.executable_digest.clone(),
        calibration_file: "calibration.json".into(),
        calibration_digest: digest_file(&calibration_file)?,
        capacity_file: "capacity.json".into(),
        capacity_digest: digest_file(&capacity_file)?,
        budget_file: budget_relative,
        budget_digest: digest_file(&budget_file)?,
        profile_file: profile_relative,
        profile_digest: digest_file(&profile_file)?,
        build_manifest_file: "build-manifest.json".into(),
        build_manifest_digest: digest_file(&build_file)?,
    };
    atomic_json(&output.join("baseline.json"), &baseline)
}

/// Converts repeatable evidence into exact workload/tuple budget entries.
fn build_budget_policy(
    id: &str,
    approved_by: &str,
    environment_id: &str,
    calibration_id: &str,
    metrics: &[CalibrationMetric],
) -> Result<BudgetPolicy> {
    validate_identifier(id)?;
    if approved_by.trim().is_empty() || environment_id.is_empty() || calibration_id.is_empty() {
        bail!("budget publication requires reviewer, environment, and calibration identity");
    }
    if metrics.is_empty() || metrics.iter().any(|metric| !metric.repeatable) {
        bail!("budget publication requires repeatable metric evidence");
    }
    let mut identities = BTreeSet::new();
    let mut entries = Vec::new();
    for metric in metrics {
        if metric.recommended_blocks == 0
            || !metric.recommended_blocks.is_multiple_of(2)
            || metric.recommended_blocks > metric.available_blocks
            || !(metric.practical_budget.is_finite() && metric.practical_budget > 0.0)
            || !(metric.target_power.is_finite()
                && metric.target_power > 0.5
                && metric.target_power < 1.0)
            || !(metric.estimated_power.is_finite()
                && metric.estimated_power >= metric.target_power
                && metric.estimated_power <= 1.0)
            || !(metric.residual_orientation_effect.is_finite()
                && metric.residual_orientation_effect <= metric.max_residual_orientation_effect)
        {
            bail!("calibration metric contains invalid reviewed evidence");
        }
        if !identities.insert((&metric.workload_id, &metric.tuple_id, &metric.metric)) {
            bail!("calibration contains a duplicate workload metric");
        }
        entries.push(BudgetEntry {
            workload_id: metric.workload_id.clone(),
            tuple_id: metric.tuple_id.clone(),
            metric: metric.metric.clone(),
            practical_budget: metric.practical_budget,
            minimum_blocks: metric.recommended_blocks,
            max_residual_orientation_effect: metric.max_residual_orientation_effect,
        });
    }
    Ok(BudgetPolicy {
        schema_id: BUDGET_SCHEMA.into(),
        id: format!("{id}-budgets"),
        environment_id: environment_id.into(),
        calibration_id: calibration_id.into(),
        approved_at: Utc::now().to_rfc3339(),
        approved_by: approved_by.trim().into(),
        entries,
    })
}

/// Selects the maximum calibrated block count needed by each workload tuple.
fn build_reference_profile(
    id: &str,
    policy: &BudgetPolicy,
    metrics: &[CalibrationMetric],
) -> Result<Profile> {
    validate_identifier(id)?;
    let mut workloads: BTreeMap<(String, String), u32> = BTreeMap::new();
    for metric in metrics {
        workloads
            .entry((metric.workload_id.clone(), metric.tuple_id.clone()))
            .and_modify(|blocks| *blocks = (*blocks).max(metric.recommended_blocks))
            .or_insert(metric.recommended_blocks);
    }
    if workloads.is_empty() {
        bail!("reference profile requires calibrated workloads");
    }
    Ok(Profile {
        schema_id: PROFILE_SCHEMA.into(),
        id: id.into(),
        description: "Canonical fixed-load regression gates derived from reviewed calibration."
            .into(),
        requires_workload_selection: false,
        campaign_cap_secs: 21_600,
        schedule_seed: 2_103_315_389,
        max_artifact_bytes: 1_073_741_824,
        max_stdout_bytes: 262_144,
        max_stderr_bytes: 262_144,
        max_residual_orientation_effect: None,
        budget_reference: Some(policy.id.clone()),
        workloads: workloads
            .into_iter()
            .map(|((workload_id, tuple), blocks)| ProfileWorkload {
                id: workload_id,
                tuple,
                blocks,
                timing: "gating".into(),
            })
            .collect(),
        extra: BTreeMap::new(),
    })
}

fn validate_identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("invalid calibrated baseline ID: {value}");
    }
    Ok(())
}

fn validate_source_documents(
    report: &ReportDocument,
    campaign: &CampaignManifest,
    environment: &EnvironmentManifest,
    kind: &str,
    profile: &str,
) -> Result<()> {
    if report.schema_id != REPORT_SCHEMA
        || campaign.schema_id != REPORT_SCHEMA
        || environment.schema_id != ENVIRONMENT_SCHEMA
    {
        bail!("calibration source uses an unsupported schema");
    }
    if report.run_id != campaign.run_id
        || report.kind != kind
        || campaign.kind != kind
        || report.profile_id != profile
        || campaign.profile_id != profile
        || report.status != "pass"
        || campaign.status != "pass"
    {
        bail!("calibration source campaign identity or terminal status is invalid");
    }
    Ok(())
}

fn load_targets(root: &Path, path: Option<&Path>) -> Result<CalibrationTargets> {
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("performance/budgets/review-targets-v1.toml"));
    let targets: CalibrationTargets = toml::from_str(
        &fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    if targets.schema_id != TARGETS_SCHEMA || targets.id.is_empty() || targets.metrics.is_empty() {
        bail!("calibration targets have an incompatible or incomplete contract");
    }
    Ok(targets)
}

fn validate_capacity_policy(policy: &CapacityPolicy) -> Result<()> {
    if ![
        policy.minimum_throughput_gain,
        policy.latency_growth,
        policy.maximum_worker_cpu_percent,
        policy.maximum_service_cpu_percent,
    ]
    .into_iter()
    .all(positive_finite)
        || policy.minimum_samples_per_point == 0
        || !(policy.minimum_scale_efficiency.is_finite()
            && (0.0..=1.0).contains(&policy.minimum_scale_efficiency))
    {
        bail!("capacity calibration policy contains an invalid limit");
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))
}

fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("parse {} line {}", path.display(), index + 1))
        })
        .collect()
}

fn median(values: &[f64]) -> f64 {
    percentile(values, 0.5)
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let index = ((values.len() as f64 * quantile).ceil() as usize)
        .saturating_sub(1)
        .min(values.len().saturating_sub(1));
    values[index]
}

/// Converts one no-change metric comparison into repeatability evidence.
fn calibrate_metric(
    workload_id: &str,
    tuple_id: &str,
    comparison: &MetricComparison,
    target: &CalibrationTarget,
) -> Result<CalibrationMetric> {
    if comparison.name != target.metric {
        bail!(
            "calibration target {} does not match comparison metric {}",
            target.metric,
            comparison.name
        );
    }
    if ![
        target.practical_budget,
        target.max_residual_orientation_effect,
    ]
    .into_iter()
    .all(positive_finite)
    {
        bail!("calibration targets must be finite and positive");
    }
    if !(target.target_power.is_finite() && target.target_power > 0.5 && target.target_power < 1.0)
    {
        bail!("calibration target power must be between 0.5 and 1.0");
    }
    let available_blocks = comparison
        .valid_abba_blocks
        .saturating_add(comparison.valid_baab_blocks);
    if available_blocks == 0 || comparison.unmatched_blocks != 0 {
        bail!("calibration requires complete balanced block evidence");
    }
    let available_blocks = u32::try_from(available_blocks)?;
    let noise_bound = comparison
        .harmful_effect
        .abs()
        .max(comparison.confidence_lower.abs())
        .max(comparison.confidence_upper.abs());
    if !noise_bound.is_finite() || !comparison.residual_orientation_effect.is_finite() {
        bail!("calibration evidence contains a non-finite effect");
    }

    // The comparison reports a two-sided 95% bootstrap interval. Treat its
    // half-width as an observed standard-error estimate and use a normal power
    // approximation to make the sample-count assumption explicit in evidence.
    let confidence_half_width =
        (comparison.confidence_upper - comparison.confidence_lower).abs() / 2.0;
    let standard_deviation = confidence_half_width / NORMAL_95 * f64::from(available_blocks).sqrt();
    let power_quantile = standard_normal_quantile(target.target_power)?;
    let scaled = if standard_deviation == 0.0 {
        2.0
    } else {
        ((NORMAL_95 + power_quantile) * standard_deviation / target.practical_budget).powi(2)
    };
    let mut recommended_blocks = scaled.ceil().max(2.0) as u32;
    if !recommended_blocks.is_multiple_of(2) {
        recommended_blocks = recommended_blocks.saturating_add(1);
    }
    let estimated_power = if standard_deviation == 0.0 {
        1.0
    } else {
        let noncentrality =
            target.practical_budget / (standard_deviation / f64::from(available_blocks).sqrt());
        standard_normal_cdf(-NORMAL_95 - noncentrality) + 1.0
            - standard_normal_cdf(NORMAL_95 - noncentrality)
    };
    let repeatable = comparison.confidence_lower <= 0.0
        && comparison.confidence_upper >= 0.0
        && comparison.residual_orientation_effect <= target.max_residual_orientation_effect
        && estimated_power >= target.target_power
        && recommended_blocks <= available_blocks;

    Ok(CalibrationMetric {
        workload_id: workload_id.into(),
        tuple_id: tuple_id.into(),
        metric: comparison.name.clone(),
        observed_effect: comparison.harmful_effect,
        confidence_lower: comparison.confidence_lower,
        confidence_upper: comparison.confidence_upper,
        noise_bound,
        practical_budget: target.practical_budget,
        available_blocks,
        recommended_blocks,
        target_power: target.target_power,
        estimated_power,
        residual_orientation_effect: comparison.residual_orientation_effect,
        max_residual_orientation_effect: target.max_residual_orientation_effect,
        repeatable,
    })
}

const NORMAL_95: f64 = 1.959_963_984_540_054;

/// Approximates the standard normal cumulative distribution without adding a statistics runtime.
fn standard_normal_cdf(value: f64) -> f64 {
    let absolute = value.abs();
    let scale = 1.0 / (1.0 + 0.231_641_9 * absolute);
    let density = (-absolute * absolute / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt();
    let tail = density
        * scale
        * (0.319_381_530
            + scale
                * (-0.356_563_782
                    + scale * (1.781_477_937 + scale * (-1.821_255_978 + scale * 1.330_274_429))));
    if value >= 0.0 { 1.0 - tail } else { tail }
}

/// Finds a standard-normal quantile for a probability strictly between zero and one.
fn standard_normal_quantile(probability: f64) -> Result<f64> {
    if !(probability.is_finite() && probability > 0.0 && probability < 1.0) {
        bail!("normal quantile probability must be between zero and one");
    }
    let (mut lower, mut upper) = (-8.0, 8.0);
    for _ in 0..64 {
        let midpoint = (lower + upper) / 2.0;
        if standard_normal_cdf(midpoint) < probability {
            lower = midpoint;
        } else {
            upper = midpoint;
        }
    }
    Ok((lower + upper) / 2.0)
}

fn positive_finite(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

/// Locates the earliest capacity knee without treating shared-service saturation as product capacity.
pub(super) fn find_knee(
    workers: u32,
    points: &[CapacityPoint],
    minimum_throughput_gain: f64,
    latency_growth: f64,
    maximum_worker_cpu_percent: f64,
    maximum_service_cpu_percent: f64,
) -> Result<CapacityKnee> {
    if workers == 0
        || !(minimum_throughput_gain.is_finite() && minimum_throughput_gain > 0.0)
        || !(latency_growth.is_finite() && latency_growth > 0.0)
        || !(maximum_worker_cpu_percent.is_finite() && maximum_worker_cpu_percent > 0.0)
        || !(maximum_service_cpu_percent.is_finite() && maximum_service_cpu_percent > 0.0)
    {
        bail!("capacity policy values must be finite and positive");
    }
    let mut points = points
        .iter()
        .filter(|point| point.workers == workers)
        .collect::<Vec<_>>();
    points.sort_by_key(|point| point.load_step);
    if points.is_empty() {
        bail!("capacity staircase has no points for {workers} worker(s)");
    }
    for (index, point) in points.iter().enumerate() {
        if point.load_step == 0
            || point.cases == 0
            || point.samples == 0
            || !(point.throughput_per_second.is_finite() && point.throughput_per_second > 0.0)
            || !(point.p95_latency_ms.is_finite() && point.p95_latency_ms > 0.0)
            || !(point.process_cpu_percent_per_worker.is_finite()
                && point.process_cpu_percent_per_worker >= 0.0)
        {
            bail!("capacity point contains a zero or non-finite value");
        }
        if index > 0 && points[index - 1].load_step >= point.load_step {
            bail!("capacity load steps must be unique and increasing");
        }
        if point
            .service_cpu_percent
            .is_some_and(|cpu| cpu >= maximum_service_cpu_percent)
        {
            bail!(
                "shared services reached {:.1}% CPU at load step {}; capacity is invalid",
                point.service_cpu_percent.unwrap_or_default(),
                point.load_step
            );
        }
    }
    for (index, current) in points.iter().enumerate() {
        let cpu_limited = current.process_cpu_percent_per_worker >= maximum_worker_cpu_percent;
        let throughput_and_latency_limited = index > 0 && {
            let previous = points[index - 1];
            let throughput_gain =
                current.throughput_per_second / previous.throughput_per_second - 1.0;
            let latency_increase = current.p95_latency_ms / previous.p95_latency_ms - 1.0;
            throughput_gain < minimum_throughput_gain && latency_increase > latency_growth
        };
        if cpu_limited || throughput_and_latency_limited {
            return Ok(CapacityKnee {
                workers,
                knee_step: Some(current.load_step),
                knee_throughput_per_second: Some(current.throughput_per_second),
                observed_rate_lower_bound: None,
            });
        }
    }

    Ok(CapacityKnee {
        workers,
        knee_step: None,
        knee_throughput_per_second: None,
        observed_rate_lower_bound: points
            .iter()
            .map(|point| point.throughput_per_second)
            .max_by(f64::total_cmp),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
    };

    use super::*;
    use crate::perf::{
        artifact::atomic_jsonl,
        config::load_profile,
        model::{
            BinaryRole,
            COMPARISON_SCHEMA,
            ComparisonDocument,
            ExternalMeasurements,
            Orientation,
            ProcessMeasurement,
            SetupAsset,
            Validation,
            Verdict,
        },
    };

    fn comparison(effect: f64, lower: f64, upper: f64) -> MetricComparison {
        MetricComparison {
            name: "wall_time".into(),
            unit: "nanoseconds".into(),
            direction: "positive_is_harmful".into(),
            baseline_median: 1.0,
            candidate_median: 1.0 + effect,
            raw_candidate_delta: effect,
            harmful_effect: effect,
            confidence_lower: lower,
            confidence_upper: upper,
            practical_budget: None,
            verdict: Verdict::Informative,
            valid_abba_blocks: 15,
            valid_baab_blocks: 15,
            unmatched_blocks: 0,
            residual_orientation_effect: 0.01,
            orientation_medians: BTreeMap::new(),
            position_medians: BTreeMap::new(),
            estimator: "test".into(),
            bootstrap_seed: 1,
        }
    }

    fn target() -> CalibrationTarget {
        CalibrationTarget {
            metric: "wall_time".into(),
            practical_budget: 0.05,
            target_power: 0.80,
            max_residual_orientation_effect: 0.02,
        }
    }

    fn environment(canonical: bool) -> EnvironmentManifest {
        EnvironmentManifest {
            schema_id: ENVIRONMENT_SCHEMA.into(),
            created_at: "2026-08-21T00:00:00Z".into(),
            environment_id: if canonical {
                "aws-m6i-2xlarge-al2023-v1"
            } else {
                "local-test"
            }
            .into(),
            canonical,
            os: "test".into(),
            architecture: "test".into(),
            logical_cpus: 8,
            hostname: None,
            collector: "test".into(),
            validity: Vec::new(),
            extra: BTreeMap::new(),
        }
    }

    fn report(
        kind: &str,
        profile_id: &str,
        comparisons: Vec<ComparisonDocument>,
    ) -> ReportDocument {
        ReportDocument {
            schema_id: REPORT_SCHEMA.into(),
            run_id: "calibration-test".into(),
            kind: kind.into(),
            status: "pass".into(),
            profile_id: profile_id.into(),
            generated_at: "2026-08-21T00:00:00Z".into(),
            comparisons,
            failures: Vec::new(),
            artifact_files: Vec::new(),
            extra: BTreeMap::new(),
        }
    }

    fn campaign(
        kind: &str,
        profile_id: &str,
        selected_workloads: Vec<String>,
        frozen: &super::super::provenance::FrozenPaths,
    ) -> CampaignManifest {
        CampaignManifest {
            schema_id: REPORT_SCHEMA.into(),
            run_id: "calibration-test".into(),
            kind: kind.into(),
            status: "pass".into(),
            created_at: "2026-08-21T00:00:00Z".into(),
            completed_at: Some("2026-08-21T00:01:00Z".into()),
            profile_id: profile_id.into(),
            schedule_seed: 1,
            selected_workloads,
            planned_measured_executions: 0,
            planned_preconditioning_executions: 0,
            artifact_limit_bytes: 1_000_000,
            environment_file: "environment.json".into(),
            registry_file: frozen.registry_file.clone(),
            profile_file: frozen.profile_file.clone(),
            budget_policy_file: frozen.budget_policy_file.clone(),
            baseline_manifest: frozen.baseline_manifest.clone(),
            candidate_manifest: frozen.candidate_manifest.clone(),
            failure: None,
            extra: BTreeMap::new(),
        }
    }

    fn build_manifest(digest: &str) -> BuildManifest {
        BuildManifest {
            schema_id: BUILD_SCHEMA.into(),
            created_at: "2026-08-21T00:00:00Z".into(),
            executable_name: "vigilo".into(),
            executable_digest: digest.into(),
            executable_bytes: 1,
            source_commit: Some("commit".into()),
            source_dirty: false,
            source_label: "test".into(),
            cargo_lock_digest: "lock".into(),
            dependency_tree_digest: "dependencies".into(),
            migrations_digest: "migrations".into(),
            evaluator_abi_digest: "abi".into(),
            rustc: "rustc test".into(),
            cargo: "cargo test".into(),
            target: "test-target".into(),
            profile: "release".into(),
            capabilities: Vec::new(),
            setup_assets: Vec::<SetupAsset>::new(),
            extra: BTreeMap::new(),
        }
    }

    fn calibration_metric(workload_id: &str, tuple_id: &str) -> CalibrationMetric {
        CalibrationMetric {
            workload_id: workload_id.into(),
            tuple_id: tuple_id.into(),
            metric: "wall_time".into(),
            observed_effect: 0.0,
            confidence_lower: -0.01,
            confidence_upper: 0.01,
            noise_bound: 0.01,
            practical_budget: 0.05,
            available_blocks: 30,
            recommended_blocks: 6,
            target_power: 0.80,
            estimated_power: 0.95,
            residual_orientation_effect: 0.01,
            max_residual_orientation_effect: 0.02,
            repeatable: true,
        }
    }

    fn complete_capacity_points() -> Vec<CapacityPoint> {
        [1, 2]
            .into_iter()
            .flat_map(|workers| {
                [1, 2, 4, 8, 16]
                    .into_iter()
                    .map(move |load_step| CapacityPoint {
                        workers,
                        load_step,
                        cases: u64::from(load_step) * 100,
                        samples: 8,
                        throughput_per_second: f64::from(load_step) * 100.0,
                        p95_latency_ms: 100.0,
                        process_cpu_percent_per_worker: 50.0,
                        service_cpu_percent: Some(30.0),
                    })
            })
            .collect()
    }

    #[test]
    fn stable_noise_supports_a_budget_and_noisy_evidence_does_not() {
        let stable = calibrate_metric(
            "startup.cli-help.v1",
            "cold-help",
            &comparison(0.0, -0.01, 0.01),
            &target(),
        )
        .unwrap();
        assert!(stable.repeatable);
        assert_eq!(stable.available_blocks, 30);
        assert!(stable.recommended_blocks <= stable.available_blocks);
        assert!(stable.estimated_power >= stable.target_power);

        let noisy = calibrate_metric(
            "startup.cli-help.v1",
            "cold-help",
            &comparison(0.04, -0.03, 0.08),
            &target(),
        )
        .unwrap();
        assert!(!noisy.repeatable);
        assert!(noisy.recommended_blocks > noisy.available_blocks);
    }

    #[test]
    fn power_quantile_is_bounded_and_reversible() {
        let quantile = standard_normal_quantile(0.80).unwrap();
        assert!((quantile - 0.841_621).abs() < 0.000_01);
        assert!((standard_normal_cdf(quantile) - 0.80).abs() < 0.000_001);
        assert!(standard_normal_quantile(0.0).is_err());
        assert!(standard_normal_quantile(1.0).is_err());
    }

    #[test]
    fn capacity_knee_is_distinct_from_shared_service_saturation() {
        let points = vec![
            CapacityPoint {
                workers: 1,
                load_step: 1,
                cases: 100,
                samples: 8,
                throughput_per_second: 100.0,
                p95_latency_ms: 100.0,
                process_cpu_percent_per_worker: 50.0,
                service_cpu_percent: Some(30.0),
            },
            CapacityPoint {
                workers: 1,
                load_step: 2,
                cases: 200,
                samples: 8,
                throughput_per_second: 108.0,
                p95_latency_ms: 130.0,
                process_cpu_percent_per_worker: 60.0,
                service_cpu_percent: Some(40.0),
            },
        ];
        let knee = find_knee(1, &points, 0.10, 0.25, 90.0, 85.0).unwrap();
        assert_eq!(knee.knee_step, Some(2));
        assert_eq!(knee.knee_throughput_per_second, Some(108.0));

        let saturated = vec![CapacityPoint {
            service_cpu_percent: Some(90.0),
            ..points[1].clone()
        }];
        assert!(find_knee(1, &saturated, 0.10, 0.25, 90.0, 85.0).is_err());

        let cpu_limited = vec![CapacityPoint {
            process_cpu_percent_per_worker: 90.0,
            ..points[0].clone()
        }];
        assert_eq!(
            find_knee(1, &cpu_limited, 0.10, 0.25, 90.0, 85.0)
                .unwrap()
                .knee_step,
            Some(1)
        );

        let mut cpu_before_shared_saturation = points;
        cpu_before_shared_saturation[0].process_cpu_percent_per_worker = 90.0;
        cpu_before_shared_saturation[1].service_cpu_percent = Some(90.0);
        assert!(find_knee(1, &cpu_before_shared_saturation, 0.10, 0.25, 90.0, 85.0).is_err());
    }

    #[test]
    fn capacity_knee_uses_the_earliest_load_that_triggers_either_rule() {
        let points = vec![
            CapacityPoint {
                workers: 1,
                load_step: 1,
                cases: 100,
                samples: 8,
                throughput_per_second: 100.0,
                p95_latency_ms: 100.0,
                process_cpu_percent_per_worker: 50.0,
                service_cpu_percent: Some(30.0),
            },
            CapacityPoint {
                workers: 1,
                load_step: 2,
                cases: 200,
                samples: 8,
                throughput_per_second: 105.0,
                p95_latency_ms: 130.0,
                process_cpu_percent_per_worker: 60.0,
                service_cpu_percent: Some(40.0),
            },
            CapacityPoint {
                workers: 1,
                load_step: 4,
                cases: 400,
                samples: 8,
                throughput_per_second: 110.0,
                p95_latency_ms: 140.0,
                process_cpu_percent_per_worker: 95.0,
                service_cpu_percent: Some(50.0),
            },
        ];

        let knee = find_knee(1, &points, 0.10, 0.25, 90.0, 85.0).unwrap();

        assert_eq!(knee.knee_step, Some(2));
        assert_eq!(knee.knee_throughput_per_second, Some(105.0));
    }

    #[test]
    fn no_observed_knee_is_reported_only_as_a_lower_bound() {
        let points = [1, 2, 4]
            .into_iter()
            .map(|step| CapacityPoint {
                workers: 1,
                load_step: step,
                cases: u64::from(step) * 100,
                samples: 8,
                throughput_per_second: f64::from(step) * 100.0,
                p95_latency_ms: 100.0,
                process_cpu_percent_per_worker: 50.0,
                service_cpu_percent: Some(30.0),
            })
            .collect::<Vec<_>>();
        let result = find_knee(1, &points, 0.10, 0.25, 90.0, 85.0).unwrap();
        assert_eq!(result.knee_step, None);
        assert_eq!(result.observed_rate_lower_bound, Some(400.0));
    }

    #[test]
    fn publication_derives_versioned_budgets_and_reference_profile() {
        let metrics = vec![calibration_metric("startup.cli-help.v1", "cold-help")];
        let policy = build_budget_policy(
            "reference-v2",
            "reviewer",
            "aws-m6i-2xlarge-al2023-v1",
            "calibration-1",
            &metrics,
        )
        .unwrap();
        assert_eq!(policy.id, "reference-v2-budgets");
        assert_eq!(policy.entries[0].minimum_blocks, 6);
        let encoded_policy = toml::to_string_pretty(&policy).unwrap();
        let decoded_policy: BudgetPolicy = toml::from_str(&encoded_policy).unwrap();
        assert_eq!(decoded_policy.entries.len(), 1);

        let profile = build_reference_profile("reference-v2", &policy, &metrics).unwrap();
        assert_eq!(
            profile.budget_reference.as_deref(),
            Some("reference-v2-budgets")
        );
        assert_eq!(profile.workloads[0].timing, "gating");
        assert_eq!(profile.workloads[0].blocks, 6);
        let encoded = toml::to_string_pretty(&profile).unwrap();
        let decoded: Profile = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.id, "reference-v2");

        let mut invalid = metrics;
        invalid[0].repeatable = false;
        assert!(
            build_budget_policy(
                "reference-v2",
                "reviewer",
                "aws-m6i-2xlarge-al2023-v1",
                "calibration-1",
                &invalid,
            )
            .is_err()
        );
    }

    #[test]
    fn noise_analysis_writes_publishable_and_noncanonical_evidence() {
        let root = workspace_root().unwrap();
        let profile = load_profile(&root, "calibration-v1").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let comparisons = profile
            .workloads
            .iter()
            .map(|workload| ComparisonDocument {
                schema_id: COMPARISON_SCHEMA.into(),
                run_id: "calibration-test".into(),
                profile_id: profile.id.clone(),
                workload_id: workload.id.clone(),
                tuple_id: workload.tuple.clone(),
                baseline_digest: "same-build".into(),
                candidate_digest: "same-build".into(),
                metrics: vec![comparison(0.0, -0.01, 0.01)],
                verdict: Verdict::Informative,
                extra: BTreeMap::new(),
            })
            .collect::<Vec<_>>();
        let selected = profile
            .workloads
            .iter()
            .map(|workload| format!("{}:{}", workload.id, workload.tuple))
            .collect();
        let registry = super::super::config::load_registry(&root).unwrap();
        let manifest = build_manifest("same-build");
        let frozen = provenance::freeze(
            directory.path(),
            &registry,
            &profile,
            None,
            Some(&manifest),
            &manifest,
        )
        .unwrap();
        atomic_json(
            &directory.path().join("report.json"),
            &report("compare", &profile.id, comparisons),
        )
        .unwrap();
        atomic_json(
            &directory.path().join("campaign.json"),
            &campaign("compare", &profile.id, selected, &frozen),
        )
        .unwrap();
        atomic_json(
            &directory.path().join("environment.json"),
            &environment(true),
        )
        .unwrap();

        let exit = analyze_noise_run(
            &root,
            NoiseArgs {
                run_dir: directory.path().into(),
                targets: None,
                output: None,
            },
        )
        .unwrap();
        assert_eq!(exit, EXIT_PASS);
        let canonical: CalibrationDocument =
            read_json(&directory.path().join("calibration.json")).unwrap();
        assert!(canonical.publishable);
        assert_eq!(canonical.metrics.len(), profile.workloads.len());

        atomic_json(
            &directory.path().join("environment.json"),
            &environment(false),
        )
        .unwrap();
        let local_output = directory.path().join("local-calibration.json");
        let exit = analyze_noise_run(
            &root,
            NoiseArgs {
                run_dir: directory.path().into(),
                targets: None,
                output: Some(local_output.clone()),
            },
        )
        .unwrap();
        assert_eq!(exit, EXIT_INVALID);
        let local: CalibrationDocument = read_json(&local_output).unwrap();
        assert!(!local.publishable);
        assert_eq!(local.failures.len(), 1);
    }

    #[test]
    fn capacity_analysis_reduces_exact_samples_and_rejects_invalid_samples() {
        let root = workspace_root().unwrap();
        let profile = load_profile(&root, "capacity-v1").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let registry = super::super::config::load_registry(&root).unwrap();
        let manifest = build_manifest("capacity-build");
        let frozen =
            provenance::freeze(directory.path(), &registry, &profile, None, None, &manifest)
                .unwrap();
        let selected = profile
            .workloads
            .iter()
            .map(|workload| format!("{}:{}", workload.id, workload.tuple))
            .collect();
        atomic_json(
            &directory.path().join("report.json"),
            &report("run", &profile.id, Vec::new()),
        )
        .unwrap();
        atomic_json(
            &directory.path().join("campaign.json"),
            &campaign("run", &profile.id, selected, &frozen),
        )
        .unwrap();
        atomic_json(
            &directory.path().join("environment.json"),
            &environment(false),
        )
        .unwrap();

        let mut samples = Vec::new();
        for workload in &profile.workloads {
            let (_, load_step) = capacity_tuple(&workload.tuple).unwrap();
            let cases = i64::try_from(load_step * 100).unwrap();
            for block_id in 0..8 {
                let mut external = ExternalMeasurements::default();
                external.durable_counts.insert("executions".into(), cases);
                external.service_cpu_percent = Some(30.0);
                samples.push(Sample {
                    schema_id: SAMPLE_SCHEMA.into(),
                    run_id: "calibration-test".into(),
                    profile_id: profile.id.clone(),
                    workload_id: workload.id.clone(),
                    tuple_id: workload.tuple.clone(),
                    block_id,
                    orientation_set_id: block_id,
                    orientation: Orientation::Abba,
                    pair_id: 0,
                    position: 1,
                    role: BinaryRole::Single,
                    measured: true,
                    started_at: "2026-08-21T00:00:00Z".into(),
                    process: ProcessMeasurement {
                        wall_time_ns: 1_000_000_000,
                        cpu_time_ns: Some(1),
                        peak_rss_bytes: Some(1),
                        stdout_first_byte_ns: None,
                        stdout_last_byte_ns: None,
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
                        code: "valid".into(),
                        message: "valid".into(),
                    },
                    external,
                    extra: BTreeMap::new(),
                });
            }
        }
        atomic_jsonl(&directory.path().join("samples.jsonl"), &samples).unwrap();

        let exit = analyze_capacity_run(
            &root,
            CapacityArgs {
                run_dir: directory.path().into(),
                targets: None,
                output: None,
            },
        )
        .unwrap();
        assert_eq!(exit, EXIT_PASS);
        let evidence: CapacityDocument =
            read_json(&directory.path().join("capacity.json")).unwrap();
        assert!(!evidence.canonical);
        assert_eq!(evidence.points.len(), profile.workloads.len());
        assert_eq!(evidence.knees.len(), 2);

        samples[0].validation.state = SampleState::Invalid;
        atomic_jsonl(&directory.path().join("samples.jsonl"), &samples).unwrap();
        assert!(
            analyze_capacity_run(
                &root,
                CapacityArgs {
                    run_dir: directory.path().into(),
                    targets: None,
                    output: Some(directory.path().join("invalid-capacity.json")),
                },
            )
            .is_err()
        );
    }

    #[test]
    fn publication_freezes_digested_artifacts_and_refuses_overwrite() {
        let root = workspace_root().unwrap();
        let source = tempfile::tempdir().unwrap();
        let digest = "published-build";
        let calibration = CalibrationDocument {
            schema_id: CALIBRATION_SCHEMA.into(),
            id: "calibration-test".into(),
            created_at: "2026-08-21T00:00:00Z".into(),
            source_run_id: "noise-run".into(),
            environment_id: "aws-m6i-2xlarge-al2023-v1".into(),
            build_digest: digest.into(),
            publishable: true,
            metrics: vec![calibration_metric("startup.cli-help.v1", "cold-help")],
            failures: Vec::new(),
        };
        let capacity_points = complete_capacity_points();
        let capacity_knees = [1, 2]
            .into_iter()
            .map(|workers| find_knee(workers, &capacity_points, 0.10, 0.25, 90.0, 85.0).unwrap())
            .collect();
        let capacity = CapacityDocument {
            schema_id: CAPACITY_SCHEMA.into(),
            source_run_id: "capacity-run".into(),
            build_digest: digest.into(),
            environment_id: calibration.environment_id.clone(),
            canonical: true,
            points: capacity_points,
            knees: capacity_knees,
            scale_efficiency_2: None,
            supports_linear_projection: false,
            failures: Vec::new(),
        };
        let targets = load_targets(&root, None).unwrap();
        validate_capacity_evidence(&capacity, &targets.capacity).unwrap();
        let mut incomplete_capacity = capacity.clone();
        incomplete_capacity.points.pop();
        assert!(validate_capacity_evidence(&incomplete_capacity, &targets.capacity).is_err());
        let calibration_path = source.path().join("calibration.json");
        let capacity_path = source.path().join("capacity.json");
        let manifest_path = source.path().join("build-manifest.json");
        atomic_json(&calibration_path, &calibration).unwrap();
        atomic_json(&capacity_path, &capacity).unwrap();
        atomic_json(&manifest_path, &build_manifest(digest)).unwrap();

        let parent = root.join("target/perf/baselines");
        fs::create_dir_all(&parent).unwrap();
        let reservation = tempfile::Builder::new()
            .prefix("calibration-publication-")
            .tempdir_in(&parent)
            .unwrap();
        let output = reservation.path().to_path_buf();
        reservation.close().unwrap();
        let id = output.file_name().unwrap().to_string_lossy().into_owned();
        let args = PublishArgs {
            calibration: calibration_path,
            capacity: capacity_path,
            build_manifest: manifest_path,
            id,
            approved_by: "reviewer".into(),
            output: Some(output.clone()),
        };

        assert_eq!(publish_baseline(&root, args.clone()).unwrap(), EXIT_PASS);
        let baseline: BaselineDocument = read_json(&output.join("baseline.json")).unwrap();
        assert_eq!(
            baseline.calibration_digest,
            digest_file(&output.join("calibration.json")).unwrap()
        );
        assert!(output.join(&baseline.budget_file).is_file());
        assert!(output.join(&baseline.profile_file).is_file());
        assert!(publish_baseline(&root, args).is_err());
        fs::remove_dir_all(output).unwrap();
    }
}
