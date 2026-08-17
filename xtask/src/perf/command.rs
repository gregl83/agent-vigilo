//! Single-binary and baseline/candidate campaign orchestration.
//!
//! Commands resolve an explicit profile, reject unavailable workloads before
//! provisioning, verify immutable build manifests and common capabilities, and
//! execute bounded schedules. They checkpoint raw samples and block validity
//! before deriving comparison and report documents.

use std::{
    fs,
    path::{
        Component,
        Path,
        PathBuf,
    },
    time::{
        Duration,
        Instant,
    },
};

use anyhow::{
    Context,
    Result,
    bail,
};
use chrono::Utc;
use clap::Args;
use serde::{
    Deserialize,
    Serialize,
};

use super::{
    EXIT_INVALID,
    EXIT_PASS,
    EXIT_REGRESSION,
    artifact::{
        CampaignLease,
        atomic_json,
        atomic_jsonl,
        atomic_text,
        create_run_dir,
        digest_file,
        digest_tree,
        directory_bytes,
        no_extra,
        run_id,
        workspace_root,
    },
    config::{
        SelectedWorkload,
        estimated_compare_millis,
        estimated_single_millis,
        load_profile,
        load_registry,
        select_workloads,
    },
    model::{
        BUILD_SCHEMA,
        BinaryRole,
        BlockRecord,
        BuildManifest,
        CampaignManifest,
        ComparisonDocument,
        EnvironmentManifest,
        ImplementationStatus,
        Orientation,
        Preconditioning,
        ProcessMeasurement,
        REPORT_SCHEMA,
        ReportDocument,
        SAMPLE_SCHEMA,
        Sample,
        SampleState,
        Validation,
        Verdict,
    },
    process::{
        ProcessOutcome,
        ProcessSpec,
        executable_path,
        execute,
    },
    report,
    schedule,
    stats::compare_wall_time,
};

/// CLI arguments for measuring one immutable release binary.
#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(long)]
    profile: String,
    #[arg(long = "workload")]
    workloads: Vec<String>,
    #[arg(long = "bin")]
    binary: PathBuf,
    #[arg(long = "build-manifest")]
    build_manifest: PathBuf,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    schedule_seed: Option<u64>,
}

/// CLI arguments for a counterbalanced baseline/candidate comparison.
#[derive(Debug, Args)]
pub struct CompareArgs {
    #[arg(long)]
    profile: String,
    #[arg(long = "workload")]
    workloads: Vec<String>,
    #[arg(long = "baseline-bin")]
    baseline_binary: PathBuf,
    #[arg(long = "baseline-build-manifest")]
    baseline_manifest: PathBuf,
    #[arg(long = "candidate-bin")]
    candidate_binary: PathBuf,
    #[arg(long = "candidate-build-manifest")]
    candidate_manifest: PathBuf,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    schedule_seed: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReadinessEvent {
    schema_id: String,
    run_id: String,
    workload_id: Option<String>,
    role: Option<BinaryRole>,
    kind: String,
    valid: bool,
    measured: bool,
    at: String,
    message: String,
}

struct ExecutedSample {
    sample: Sample,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Executes an informative single-binary campaign for the selected profile.
pub fn run_single(args: RunArgs) -> Result<u8> {
    let root = workspace_root()?;
    let registry = load_registry(&root)?;
    let profile = load_profile(&root, &args.profile)?;
    let selected = select_workloads(&profile, &registry, &args.workloads)?;
    require_implemented(&selected)?;
    let binary = executable_path(&args.binary)?;
    let manifest = load_and_verify_manifest(&args.build_manifest, &binary)?;
    require_capabilities(&selected, &[&manifest])?;
    let planned_millis = estimated_single_millis(&selected);
    validate_campaign_budget(&profile, &selected, planned_millis, false)?;

    let _lease = CampaignLease::acquire(&root)?;
    let run_dir = create_run_dir(&root, args.output.as_deref(), "run")?;
    let id = run_id();
    let seed = args.schedule_seed.unwrap_or(profile.schedule_seed);
    let environment = environment_manifest();
    atomic_json(&run_dir.join("environment.json"), &environment)?;
    let mut campaign = campaign_manifest(
        &id,
        "run",
        &profile,
        seed,
        &selected,
        None,
        &args.build_manifest,
        false,
    );
    atomic_json(&run_dir.join("campaign.json"), &campaign)?;
    print_budget(&selected, planned_millis, false);

    let deadline = Instant::now() + Duration::from_secs(profile.campaign_cap_secs);
    let mut readiness = vec![ReadinessEvent {
        schema_id: REPORT_SCHEMA.into(),
        run_id: id.clone(),
        workload_id: None,
        role: None,
        kind: "campaign_readiness".into(),
        valid: true,
        measured: false,
        at: Utc::now().to_rfc3339(),
        message: "binary, manifest, profile, artifact path, and service-free workload validated"
            .into(),
    }];
    let mut samples = Vec::new();
    let mut blocks = Vec::new();
    let mut failures = Vec::new();
    let mut exit = EXIT_PASS;

    for selected_workload in &selected {
        if selected_workload.workload.preconditioning == Preconditioning::OnePerBinary {
            let precondition = execute_workload(ExecutionRequest {
                run_id: &id,
                profile_id: &profile.id,
                selected: selected_workload,
                binary: &binary,
                role: BinaryRole::Single,
                orientation: Orientation::Single,
                block_id: 0,
                pair_id: 0,
                position: 0,
                measured: false,
                stdout_limit: profile.max_stdout_bytes,
                stderr_limit: profile.max_stderr_bytes,
            })?;
            let valid = precondition.sample.validation.state == SampleState::Valid;
            readiness.push(ReadinessEvent {
                schema_id: REPORT_SCHEMA.into(),
                run_id: id.clone(),
                workload_id: Some(selected_workload.workload.id.clone()),
                role: Some(BinaryRole::Single),
                kind: "one_per_binary_preconditioning".into(),
                valid,
                measured: false,
                at: Utc::now().to_rfc3339(),
                message: precondition.sample.validation.message.clone(),
            });
            if !valid {
                failures.push(format!(
                    "{} preconditioning failed: {}",
                    selected_workload.workload.id, precondition.sample.validation.message
                ));
                exit = classification_exit(&precondition.sample);
                write_failure_output(&run_dir, &precondition, "preconditioning")?;
                break;
            }
        }

        for block_id in 0..selected_workload.profile.blocks {
            ensure_before_deadline(deadline)?;
            verify_executable_unchanged(&binary, &manifest)?;
            let orientation = schedule::orientation(
                seed,
                &selected_workload.workload.id,
                &selected_workload.profile.tuple,
                block_id,
            );
            let scheduled = schedule::executions(orientation);
            let mut block_samples = Vec::new();
            for execution in scheduled
                .iter()
                .filter(|execution| execution.role == BinaryRole::Candidate)
            {
                let executed = execute_workload(ExecutionRequest {
                    run_id: &id,
                    profile_id: &profile.id,
                    selected: selected_workload,
                    binary: &binary,
                    role: BinaryRole::Single,
                    orientation,
                    block_id,
                    pair_id: execution.pair_id,
                    position: execution.position,
                    measured: true,
                    stdout_limit: profile.max_stdout_bytes,
                    stderr_limit: profile.max_stderr_bytes,
                })?;
                if executed.sample.validation.state != SampleState::Valid {
                    exit = classification_exit(&executed.sample);
                    failures.push(format!(
                        "{} block {} position {}: {}",
                        selected_workload.workload.id,
                        block_id,
                        execution.position,
                        executed.sample.validation.message
                    ));
                    write_failure_output(
                        &run_dir,
                        &executed,
                        &format!("block-{block_id}-position-{}", execution.position),
                    )?;
                }
                block_samples.push(executed.sample);
                if exit != EXIT_PASS {
                    break;
                }
            }
            let valid = block_samples
                .iter()
                .all(|sample| sample.validation.state == SampleState::Valid);
            samples.extend(block_samples);
            blocks.push(BlockRecord {
                schema_id: SAMPLE_SCHEMA.into(),
                run_id: id.clone(),
                workload_id: selected_workload.workload.id.clone(),
                tuple_id: selected_workload.profile.tuple.clone(),
                block_id,
                orientation_set_id: block_id / 2,
                orientation,
                complete: valid,
                valid,
                sample_count: 2,
            });
            checkpoint(&run_dir, &samples, &blocks, &readiness)?;
            enforce_artifact_limit(&run_dir, profile.max_artifact_bytes)?;
            if exit != EXIT_PASS {
                break;
            }
        }
        if exit != EXIT_PASS {
            break;
        }
    }

    let status = if exit == EXIT_PASS {
        "pass"
    } else if exit == EXIT_REGRESSION {
        "failure"
    } else {
        "invalid"
    };
    campaign.status = status.into();
    campaign.completed_at = Some(Utc::now().to_rfc3339());
    campaign.failure = failures.first().cloned();
    atomic_json(&run_dir.join("campaign.json"), &campaign)?;
    let report = ReportDocument {
        schema_id: REPORT_SCHEMA.into(),
        run_id: id,
        kind: "run".into(),
        status: status.into(),
        profile_id: profile.id,
        generated_at: Utc::now().to_rfc3339(),
        comparisons: Vec::new(),
        failures,
        artifact_files: artifact_files(false),
        extra: no_extra(),
    };
    report::write(&run_dir, &report)?;
    Ok(exit)
}

/// Executes a counterbalanced baseline/candidate campaign and writes verdicts.
pub fn compare(args: CompareArgs) -> Result<u8> {
    let root = workspace_root()?;
    let registry = load_registry(&root)?;
    let profile = load_profile(&root, &args.profile)?;
    let selected = select_workloads(&profile, &registry, &args.workloads)?;
    require_implemented(&selected)?;
    let baseline_binary = executable_path(&args.baseline_binary)?;
    let candidate_binary = executable_path(&args.candidate_binary)?;
    let baseline_manifest = load_and_verify_manifest(&args.baseline_manifest, &baseline_binary)?;
    let candidate_manifest = load_and_verify_manifest(&args.candidate_manifest, &candidate_binary)?;
    if baseline_manifest.target != candidate_manifest.target {
        bail!(
            "build targets differ: {} versus {}",
            baseline_manifest.target,
            candidate_manifest.target
        );
    }
    require_capabilities(&selected, &[&baseline_manifest, &candidate_manifest])?;
    let planned_millis = estimated_compare_millis(&selected);
    validate_campaign_budget(&profile, &selected, planned_millis, true)?;

    let _lease = CampaignLease::acquire(&root)?;
    let run_dir = create_run_dir(&root, args.output.as_deref(), "compare")?;
    let id = run_id();
    let seed = args.schedule_seed.unwrap_or(profile.schedule_seed);
    atomic_json(&run_dir.join("environment.json"), &environment_manifest())?;
    let mut campaign = campaign_manifest(
        &id,
        "compare",
        &profile,
        seed,
        &selected,
        Some(&args.baseline_manifest),
        &args.candidate_manifest,
        true,
    );
    atomic_json(&run_dir.join("campaign.json"), &campaign)?;
    print_budget(&selected, planned_millis, true);

    let deadline = Instant::now() + Duration::from_secs(profile.campaign_cap_secs);
    let mut readiness = vec![ReadinessEvent {
        schema_id: REPORT_SCHEMA.into(),
        run_id: id.clone(),
        workload_id: None,
        role: None,
        kind: "campaign_readiness".into(),
        valid: true,
        measured: false,
        at: Utc::now().to_rfc3339(),
        message: "both immutable manifests, executable digests, common capabilities, and output limits validated".into(),
    }];
    let mut samples = Vec::new();
    let mut blocks = Vec::new();
    let mut comparisons = Vec::new();
    let mut failures = Vec::new();
    let mut exit = EXIT_PASS;

    for selected_workload in &selected {
        if selected_workload.workload.preconditioning == Preconditioning::OnePerBinary {
            let first_orientation = schedule::orientation(
                seed,
                &selected_workload.workload.id,
                &selected_workload.profile.tuple,
                0,
            );
            for role in schedule::preconditioning_order(first_orientation) {
                let binary = binary_for(role, &baseline_binary, &candidate_binary);
                let precondition = execute_workload(ExecutionRequest {
                    run_id: &id,
                    profile_id: &profile.id,
                    selected: selected_workload,
                    binary,
                    role,
                    orientation: first_orientation,
                    block_id: 0,
                    pair_id: 0,
                    position: 0,
                    measured: false,
                    stdout_limit: profile.max_stdout_bytes,
                    stderr_limit: profile.max_stderr_bytes,
                })?;
                let valid = precondition.sample.validation.state == SampleState::Valid;
                readiness.push(ReadinessEvent {
                    schema_id: REPORT_SCHEMA.into(),
                    run_id: id.clone(),
                    workload_id: Some(selected_workload.workload.id.clone()),
                    role: Some(role),
                    kind: "one_per_binary_preconditioning".into(),
                    valid,
                    measured: false,
                    at: Utc::now().to_rfc3339(),
                    message: precondition.sample.validation.message.clone(),
                });
                if !valid {
                    exit = classification_exit(&precondition.sample);
                    failures.push(format!(
                        "{} {role:?} preconditioning failed: {}",
                        selected_workload.workload.id, precondition.sample.validation.message
                    ));
                    write_failure_output(
                        &run_dir,
                        &precondition,
                        &format!("preconditioning-{role:?}"),
                    )?;
                    break;
                }
            }
            checkpoint(&run_dir, &samples, &blocks, &readiness)?;
            if exit != EXIT_PASS {
                break;
            }
        }

        let workload_sample_start = samples.len();
        for block_id in 0..selected_workload.profile.blocks {
            ensure_before_deadline(deadline)?;
            let orientation = schedule::orientation(
                seed,
                &selected_workload.workload.id,
                &selected_workload.profile.tuple,
                block_id,
            );
            for role in schedule::preconditioning_order(orientation) {
                match role {
                    BinaryRole::Baseline => {
                        verify_executable_unchanged(&baseline_binary, &baseline_manifest)?;
                    }
                    BinaryRole::Candidate => {
                        verify_executable_unchanged(&candidate_binary, &candidate_manifest)?;
                    }
                    BinaryRole::Single => unreachable!(),
                }
            }
            let mut block_samples = Vec::new();
            for execution in schedule::executions(orientation) {
                let binary = binary_for(execution.role, &baseline_binary, &candidate_binary);
                let executed = execute_workload(ExecutionRequest {
                    run_id: &id,
                    profile_id: &profile.id,
                    selected: selected_workload,
                    binary,
                    role: execution.role,
                    orientation,
                    block_id,
                    pair_id: execution.pair_id,
                    position: execution.position,
                    measured: true,
                    stdout_limit: profile.max_stdout_bytes,
                    stderr_limit: profile.max_stderr_bytes,
                })?;
                if executed.sample.validation.state != SampleState::Valid {
                    exit = classification_exit(&executed.sample);
                    failures.push(format!(
                        "{} block {} position {} ({:?}): {}",
                        selected_workload.workload.id,
                        block_id,
                        execution.position,
                        execution.role,
                        executed.sample.validation.message
                    ));
                    write_failure_output(
                        &run_dir,
                        &executed,
                        &format!("block-{block_id}-position-{}", execution.position),
                    )?;
                }
                block_samples.push(executed.sample);
                if exit != EXIT_PASS {
                    break;
                }
            }
            let complete = block_samples.len() == 4;
            let valid = complete
                && block_samples
                    .iter()
                    .all(|sample| sample.validation.state == SampleState::Valid);
            let count = block_samples.len();
            samples.extend(block_samples);
            blocks.push(BlockRecord {
                schema_id: SAMPLE_SCHEMA.into(),
                run_id: id.clone(),
                workload_id: selected_workload.workload.id.clone(),
                tuple_id: selected_workload.profile.tuple.clone(),
                block_id,
                orientation_set_id: block_id / 2,
                orientation,
                complete,
                valid,
                sample_count: count,
            });
            checkpoint(&run_dir, &samples, &blocks, &readiness)?;
            enforce_artifact_limit(&run_dir, profile.max_artifact_bytes)?;
            if exit != EXIT_PASS {
                break;
            }
        }

        if exit == EXIT_PASS {
            let workload_samples = &samples[workload_sample_start..];
            let wall_time = compare_wall_time(
                workload_samples,
                seed ^ u64::from(selected_workload.profile.blocks),
                None,
                profile.max_residual_orientation_effect,
                false,
            )?;
            if wall_time.verdict == Verdict::Invalid {
                exit = EXIT_INVALID;
                failures.push(format!(
                    "{} retained residual orientation effect {:.2}%",
                    selected_workload.workload.id,
                    wall_time.residual_orientation_effect * 100.0
                ));
            }
            let comparison = ComparisonDocument {
                schema_id: super::model::COMPARISON_SCHEMA.into(),
                run_id: id.clone(),
                profile_id: profile.id.clone(),
                workload_id: selected_workload.workload.id.clone(),
                tuple_id: selected_workload.profile.tuple.clone(),
                baseline_digest: baseline_manifest.executable_digest.clone(),
                candidate_digest: candidate_manifest.executable_digest.clone(),
                metrics: vec![wall_time],
                verdict: if exit == EXIT_INVALID {
                    Verdict::Invalid
                } else {
                    Verdict::Informative
                },
                extra: no_extra(),
            };
            let name = artifact_name(&comparison.workload_id, &comparison.tuple_id);
            atomic_json(
                &run_dir.join("comparisons").join(format!("{name}.json")),
                &comparison,
            )?;
            comparisons.push(comparison);
        }
        if exit != EXIT_PASS {
            break;
        }
    }

    let status = if exit == EXIT_PASS {
        "pass"
    } else if exit == EXIT_REGRESSION {
        "failure"
    } else {
        "invalid"
    };
    campaign.status = status.into();
    campaign.completed_at = Some(Utc::now().to_rfc3339());
    campaign.failure = failures.first().cloned();
    atomic_json(&run_dir.join("campaign.json"), &campaign)?;
    atomic_jsonl(&run_dir.join("comparisons.jsonl"), &comparisons)?;
    let report = ReportDocument {
        schema_id: REPORT_SCHEMA.into(),
        run_id: id,
        kind: "compare".into(),
        status: status.into(),
        profile_id: profile.id,
        generated_at: Utc::now().to_rfc3339(),
        comparisons,
        failures,
        artifact_files: artifact_files(true),
        extra: no_extra(),
    };
    report::write(&run_dir, &report)?;
    enforce_artifact_limit(&run_dir, profile.max_artifact_bytes)?;
    Ok(exit)
}

fn load_and_verify_manifest(path: &Path, binary: &Path) -> Result<BuildManifest> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let manifest: BuildManifest =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    if manifest.schema_id != BUILD_SCHEMA {
        bail!("unsupported build manifest schema: {}", manifest.schema_id);
    }
    let digest = digest_file(binary)?;
    if digest != manifest.executable_digest {
        bail!(
            "executable digest mismatch for {}: manifest {}, actual {}",
            binary.display(),
            manifest.executable_digest,
            digest
        );
    }
    if binary.metadata()?.len() != manifest.executable_bytes {
        bail!("executable size does not match build manifest");
    }
    let manifest_dir = path.parent().context("build manifest has no parent")?;
    for asset in &manifest.setup_assets {
        let relative = Path::new(&asset.relative_path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| component == Component::ParentDir)
        {
            bail!("unsafe setup asset path: {}", asset.relative_path);
        }
        let actual = digest_tree(&manifest_dir.join(relative))?;
        if actual != asset.digest {
            bail!("setup asset digest mismatch: {}", asset.name);
        }
    }
    Ok(manifest)
}

fn verify_executable_unchanged(binary: &Path, manifest: &BuildManifest) -> Result<()> {
    let digest = digest_file(binary)?;
    if digest != manifest.executable_digest {
        bail!(
            "executable changed during the campaign: {} expected {}, found {}",
            binary.display(),
            manifest.executable_digest,
            digest
        );
    }
    Ok(())
}

fn require_implemented(selected: &[SelectedWorkload<'_>]) -> Result<()> {
    let planned: Vec<_> = selected
        .iter()
        .filter(|selected| selected.workload.status != ImplementationStatus::Implemented)
        .map(|selected| selected.workload.id.as_str())
        .collect();
    if !planned.is_empty() {
        bail!(
            "profile selection includes workloads planned for Phase 2 or later: {}",
            planned.join(", ")
        );
    }
    Ok(())
}

fn require_capabilities(
    selected: &[SelectedWorkload<'_>],
    manifests: &[&BuildManifest],
) -> Result<()> {
    for selected in selected {
        for manifest in manifests {
            if !has_capability(&manifest.capabilities, &selected.workload.capability) {
                bail!(
                    "build {} lacks capability {}",
                    manifest.executable_digest,
                    selected.workload.capability
                );
            }
        }
    }
    Ok(())
}

fn has_capability(capabilities: &[String], required: &str) -> bool {
    capabilities.iter().any(|capability| capability == required)
}

fn validate_campaign_budget(
    profile: &super::model::Profile,
    selected: &[SelectedWorkload<'_>],
    planned_millis: u64,
    comparison: bool,
) -> Result<()> {
    if planned_millis > profile.campaign_cap_secs * 1000 {
        bail!(
            "planned duration {}s exceeds profile cap {}s",
            planned_millis.div_ceil(1000),
            profile.campaign_cap_secs
        );
    }
    let process_count: u64 = selected
        .iter()
        .map(|selected| {
            let measured = u64::from(selected.profile.blocks) * if comparison { 4 } else { 2 };
            let precondition = if selected.workload.preconditioning == Preconditioning::OnePerBinary
            {
                if comparison { 2 } else { 1 }
            } else {
                0
            };
            measured + precondition
        })
        .sum();
    let worst_case_output =
        process_count.saturating_mul((profile.max_stdout_bytes + profile.max_stderr_bytes) as u64);
    if worst_case_output > profile.max_artifact_bytes {
        bail!(
            "worst-case captured output {} bytes exceeds artifact cap {} bytes",
            worst_case_output,
            profile.max_artifact_bytes
        );
    }
    Ok(())
}

struct ExecutionRequest<'request, 'config> {
    run_id: &'request str,
    profile_id: &'request str,
    selected: &'request SelectedWorkload<'config>,
    binary: &'request Path,
    role: BinaryRole,
    orientation: Orientation,
    block_id: u32,
    pair_id: u8,
    position: u8,
    measured: bool,
    stdout_limit: usize,
    stderr_limit: usize,
}

fn execute_workload(request: ExecutionRequest<'_, '_>) -> Result<ExecutedSample> {
    let started_at = Utc::now().to_rfc3339();
    let outcome = execute(&ProcessSpec {
        program: request.binary,
        args: &request.selected.workload.command,
        current_dir: None,
        timeout: Duration::from_millis(request.selected.workload.watchdog_ms),
        stdout_limit: request.stdout_limit,
        stderr_limit: request.stderr_limit,
    })?;
    let validation = validate_outcome(&outcome, request.selected, request.role);
    let measurement = ProcessMeasurement {
        wall_time_ns: outcome.wall_time.as_nanos().min(u128::from(u64::MAX)) as u64,
        cpu_time_ns: outcome.cpu_time_ns,
        peak_rss_bytes: outcome.peak_rss_bytes,
        resource_source: outcome.resource_source.into(),
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        stdout_bytes: outcome.stdout.bytes_seen,
        stderr_bytes: outcome.stderr.bytes_seen,
        stdout_truncated: outcome.stdout.truncated,
        stderr_truncated: outcome.stderr.truncated,
    };
    Ok(ExecutedSample {
        sample: Sample {
            schema_id: SAMPLE_SCHEMA.into(),
            run_id: request.run_id.into(),
            profile_id: request.profile_id.into(),
            workload_id: request.selected.workload.id.clone(),
            tuple_id: request.selected.profile.tuple.clone(),
            block_id: request.block_id,
            orientation_set_id: request.block_id / 2,
            orientation: request.orientation,
            pair_id: request.pair_id,
            position: request.position,
            role: request.role,
            measured: request.measured,
            started_at,
            process: measurement,
            validation,
            extra: no_extra(),
        },
        stdout: outcome.stdout.data,
        stderr: outcome.stderr.data,
    })
}

fn validate_outcome(
    outcome: &ProcessOutcome,
    selected: &SelectedWorkload<'_>,
    role: BinaryRole,
) -> Validation {
    let failure_state = if role == BinaryRole::Baseline {
        SampleState::Invalid
    } else {
        SampleState::ProductFailure
    };
    if outcome.timed_out {
        return validation(
            failure_state,
            "process_timeout",
            "workload exceeded its watchdog",
        );
    }
    if outcome.exit_code != Some(0) {
        return validation(
            failure_state,
            "process_exit",
            &format!("workload exited with {:?}", outcome.exit_code),
        );
    }
    if outcome.stdout.truncated || outcome.stderr.truncated {
        return validation(
            failure_state,
            "output_truncated",
            "workload output exceeded its byte limit",
        );
    }
    let stdout = outcome.stdout.text();
    if let Some(signature) = missing_signature(&stdout, &selected.workload.help_signatures) {
        return validation(
            failure_state,
            "fixture_mismatch",
            &format!("missing expected output signature {signature:?}"),
        );
    }
    validation(SampleState::Valid, "ok", "exact startup oracle passed")
}

fn missing_signature<'a>(output: &str, signatures: &'a [String]) -> Option<&'a str> {
    signatures
        .iter()
        .find(|signature| !output.contains(signature.as_str()))
        .map(String::as_str)
}

fn validation(state: SampleState, code: &str, message: &str) -> Validation {
    Validation {
        state,
        code: code.into(),
        message: message.into(),
    }
}

fn classification_exit(sample: &Sample) -> u8 {
    match sample.validation.state {
        SampleState::Valid => EXIT_PASS,
        SampleState::ProductFailure => EXIT_REGRESSION,
        SampleState::Invalid => EXIT_INVALID,
    }
}

fn checkpoint(
    run_dir: &Path,
    samples: &[Sample],
    blocks: &[BlockRecord],
    readiness: &[ReadinessEvent],
) -> Result<()> {
    atomic_jsonl(&run_dir.join("samples.jsonl"), samples)?;
    atomic_jsonl(&run_dir.join("blocks.jsonl"), blocks)?;
    atomic_jsonl(&run_dir.join("readiness.jsonl"), readiness)
}

fn write_failure_output(run_dir: &Path, executed: &ExecutedSample, name: &str) -> Result<()> {
    let directory = run_dir
        .join("failures")
        .join(artifact_name(name, "failure"));
    atomic_json(&directory.join("sample.json"), &executed.sample)?;
    atomic_text(
        &directory.join("stdout.log"),
        &String::from_utf8_lossy(&executed.stdout),
    )?;
    atomic_text(
        &directory.join("stderr.log"),
        &String::from_utf8_lossy(&executed.stderr),
    )
}

fn binary_for<'a>(role: BinaryRole, baseline: &'a Path, candidate: &'a Path) -> &'a Path {
    match role {
        BinaryRole::Baseline => baseline,
        BinaryRole::Candidate | BinaryRole::Single => candidate,
    }
}

fn environment_manifest() -> EnvironmentManifest {
    let environment_id = std::env::var("VIGILO_PERF_ENVIRONMENT_ID").unwrap_or_else(|_| {
        format!(
            "local-{}-{}-development-v1",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    });
    let canonical = environment_id == "aws-m6i-2xlarge-al2023-v1"
        && cfg!(target_os = "linux")
        && std::env::var("VIGILO_PERF_CANONICAL_VALIDATED").as_deref() == Ok("1");
    EnvironmentManifest {
        schema_id: super::model::ENVIRONMENT_SCHEMA.into(),
        created_at: Utc::now().to_rfc3339(),
        canonical,
        environment_id,
        os: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
        logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
        hostname: std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .ok(),
        collector: if cfg!(windows) {
            "windows-job-object-v1"
        } else {
            "phase1-wall-time-v1"
        }
        .into(),
        validity: vec![
            if canonical {
                "canonical_environment_declared"
            } else {
                "noncanonical_development_mode"
            }
            .into(),
        ],
        extra: no_extra(),
    }
}

#[allow(clippy::too_many_arguments)]
fn campaign_manifest(
    id: &str,
    kind: &str,
    profile: &super::model::Profile,
    seed: u64,
    selected: &[SelectedWorkload<'_>],
    baseline_manifest: Option<&Path>,
    candidate_manifest: &Path,
    comparison: bool,
) -> CampaignManifest {
    let measured = selected
        .iter()
        .map(|selected| u64::from(selected.profile.blocks) * if comparison { 4 } else { 2 })
        .sum();
    let precondition = selected
        .iter()
        .filter(|selected| selected.workload.preconditioning == Preconditioning::OnePerBinary)
        .count() as u64
        * if comparison { 2 } else { 1 };
    CampaignManifest {
        schema_id: REPORT_SCHEMA.into(),
        run_id: id.into(),
        kind: kind.into(),
        status: "running".into(),
        created_at: Utc::now().to_rfc3339(),
        completed_at: None,
        profile_id: profile.id.clone(),
        schedule_seed: seed,
        selected_workloads: selected
            .iter()
            .map(|selected| format!("{}:{}", selected.workload.id, selected.profile.tuple))
            .collect(),
        planned_measured_executions: measured,
        planned_preconditioning_executions: precondition,
        artifact_limit_bytes: profile.max_artifact_bytes,
        environment_file: "environment.json".into(),
        baseline_manifest: baseline_manifest.map(|path| path.display().to_string()),
        candidate_manifest: candidate_manifest.display().to_string(),
        failure: None,
        extra: no_extra(),
    }
}

fn ensure_before_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        bail!("campaign wall-time cap expired before the next block");
    }
    Ok(())
}

fn enforce_artifact_limit(run_dir: &Path, limit: u64) -> Result<()> {
    let bytes = directory_bytes(run_dir)?;
    if bytes > limit {
        bail!("campaign artifact limit exceeded: {bytes} > {limit} bytes");
    }
    Ok(())
}

fn print_budget(selected: &[SelectedWorkload<'_>], planned_millis: u64, comparison: bool) {
    println!("Execution budget:");
    for selected in selected {
        let measured = selected.profile.blocks * if comparison { 4 } else { 2 };
        let discarded = if selected.workload.preconditioning == Preconditioning::OnePerBinary {
            if comparison { 2 } else { 1 }
        } else {
            0
        };
        println!(
            "  {}:{}  blocks={} measured={} preconditioning={}",
            selected.workload.id,
            selected.profile.tuple,
            selected.profile.blocks,
            measured,
            discarded
        );
    }
    println!(
        "  planned campaign duration: {:.1}s",
        planned_millis as f64 / 1000.0
    );
}

fn artifact_name(left: &str, right: &str) -> String {
    format!("{left}-{right}")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn artifact_files(comparison: bool) -> Vec<String> {
    let mut files = vec![
        "campaign.json".into(),
        "environment.json".into(),
        "readiness.jsonl".into(),
        "samples.jsonl".into(),
        "blocks.jsonl".into(),
        "report.json".into(),
        "summary.md".into(),
    ];
    if comparison {
        files.insert(5, "comparisons.jsonl".into());
    }
    files
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn candidate_failures_are_product_failures_but_baseline_failures_are_invalid() {
        let base = Sample {
            schema_id: SAMPLE_SCHEMA.into(),
            run_id: "run".into(),
            profile_id: "profile".into(),
            workload_id: "workload".into(),
            tuple_id: "tuple".into(),
            block_id: 0,
            orientation_set_id: 0,
            orientation: Orientation::Abba,
            pair_id: 0,
            position: 1,
            role: BinaryRole::Baseline,
            measured: true,
            started_at: "now".into(),
            process: ProcessMeasurement {
                wall_time_ns: 1,
                cpu_time_ns: None,
                peak_rss_bytes: None,
                resource_source: "test".into(),
                exit_code: Some(1),
                timed_out: false,
                stdout_bytes: 0,
                stderr_bytes: 0,
                stdout_truncated: false,
                stderr_truncated: false,
            },
            validation: Validation {
                state: SampleState::Invalid,
                code: "process_exit".into(),
                message: "failed".into(),
            },
            extra: BTreeMap::new(),
        };
        assert_eq!(classification_exit(&base), EXIT_INVALID);
        let mut candidate = base.clone();
        candidate.role = BinaryRole::Candidate;
        candidate.validation.state = SampleState::ProductFailure;
        assert_eq!(classification_exit(&candidate), EXIT_REGRESSION);
    }

    #[test]
    fn artifact_names_do_not_create_paths() {
        assert_eq!(artifact_name("a/b", "c:d"), "a-b-c-d");
    }

    #[test]
    fn capability_mismatch_is_detected_symmetrically() {
        let baseline = vec!["startup.cli-help.v1".to_owned()];
        let candidate = vec!["run.create.v1".to_owned()];
        assert!(has_capability(&baseline, "startup.cli-help.v1"));
        assert!(!has_capability(&candidate, "startup.cli-help.v1"));
    }

    #[test]
    fn fixture_signature_mismatch_is_detected() {
        let signatures = vec!["Usage:".to_owned(), "Commands:".to_owned()];
        assert_eq!(
            missing_signature("Usage: vigilo", &signatures),
            Some("Commands:")
        );
        assert_eq!(
            missing_signature("Usage: vigilo\nCommands:", &signatures),
            None
        );
    }
}
