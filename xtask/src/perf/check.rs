//! Static and service-free validation of performance-harness contracts.
//!
//! Checks cover registry/profile consistency, audited production constants,
//! dependency and package isolation, artifact containment, endpoint ownership,
//! and subprocess cleanup. No benchmark services are provisioned here.

use std::{
    fs,
    path::Path,
    process::Command,
    time::Duration,
};

use anyhow::{
    Context,
    Result,
    bail,
};
use clap::Args;
use serde_json::Value;

use super::{
    EXIT_PASS,
    artifact::{
        require_artifact_path,
        workspace_root,
    },
    config::{
        load_profile,
        load_registry,
        validate_profile_registry,
    },
    model::ImplementationStatus,
    process::{
        ProcessSpec,
        execute as execute_process,
    },
    schedule,
};

const PROFILES: [&str; 4] = ["developer-v1", "pr-v1", "reference-v1", "calibration-v1"];

/// CLI arguments for validating the performance harness.
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Git base used to prove a preparatory harness change leaves product code untouched.
    #[arg(long)]
    bootstrap_base: Option<String>,
    /// Destructive endpoint to validate; Phase 1 never connects to it.
    #[arg(long)]
    endpoint: Vec<String>,
    /// Run-specific marker that must be embedded in every supplied endpoint.
    #[arg(long)]
    ownership_marker: Option<String>,
}

/// Runs all service-free harness, provenance, and isolation checks.
pub fn execute(args: CheckArgs) -> Result<u8> {
    let root = workspace_root()?;
    let registry = load_registry(&root)?;
    validate_constants(&registry.constants)?;
    validate_environment_contract(&root)?;
    validate_external_tools(&root)?;
    for id in PROFILES {
        let profile = load_profile(&root, id)?;
        validate_profile_registry(&profile, &registry)?;
        for workload in &profile.workloads {
            schedule::validate(
                profile
                    .workloads
                    .iter()
                    .find(|item| item.id == workload.id && item.tuple == workload.tuple)
                    .expect("profile workload exists")
                    .blocks,
                profile.schedule_seed,
                &workload.id,
                &workload.tuple,
            )?;
        }
    }
    validate_profile_implementation_contract(&registry)?;
    validate_dependency_boundaries(&root)?;
    validate_package_contents(&root)?;
    validate_fixture_tree(&root)?;
    if let Some(base) = args.bootstrap_base.as_deref() {
        validate_bootstrap_delta(&root, base)?;
    }
    validate_endpoints(&args.endpoint, args.ownership_marker.as_deref())?;
    let _ = require_artifact_path(&root, Path::new("target/perf/runs/check"))?;
    process_self_test()?;

    println!("Performance harness check passed");
    println!(
        "  registry:    workload-registry/v1 ({} workloads)",
        registry.workloads.len()
    );
    println!("  profiles:    {}", PROFILES.join(", "));
    println!("  tools:       cargo, rustc, and git available");
    println!("  production:  no reverse dependency or package leakage");
    println!("  process:     timeout, truncation, exit, and cleanup self-test passed");
    println!("  services:    no Phase 1 service endpoint is opened or mutated");
    Ok(EXIT_PASS)
}

fn validate_environment_contract(root: &Path) -> Result<()> {
    let path = root.join("performance/environments/aws-m6i-2xlarge-al2023-v1.toml");
    let environment: toml::Value = toml::from_str(
        &fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
    )?;
    if environment.get("schema_id").and_then(toml::Value::as_str)
        != Some(super::model::ENVIRONMENT_SCHEMA)
        || environment.get("id").and_then(toml::Value::as_str) != Some("aws-m6i-2xlarge-al2023-v1")
        || environment.get("canonical").and_then(toml::Value::as_bool) != Some(true)
    {
        bail!("canonical performance environment contract is invalid");
    }
    Ok(())
}

fn validate_external_tools(root: &Path) -> Result<()> {
    for (program, args) in [
        ("cargo", &["-V"][..]),
        ("rustc", &["-V"][..]),
        ("git", &["--version"][..]),
    ] {
        command_output(root, program, args)?;
    }
    Ok(())
}

fn validate_constants(constants: &super::model::RegistryConstants) -> Result<()> {
    let values = [
        constants.database_connections_per_target as u64,
        constants.database_acquire_timeout_ms,
        constants.database_operation_deadline_ms,
        constants.run_chunk_size as u64,
        constants.creation_case_page_size as u64,
        constants.creation_page_budget as u64,
        constants.case_blob_group_size as u64,
        constants.membership_group_size as u64,
        constants.chunk_insert_group_size as u64,
        constants.coordinator_tick_ms,
        constants.coordinator_create_recovery_budget as u64,
        constants.coordinator_dispatch_budget as u64,
        constants.coordinator_finalization_budget as u64,
        constants.dispatch_window_size as u64,
        constants.lease_recovery_batch_size as u64,
        constants.outbox_batch_size as u64,
        constants.outbox_publish_parallelism as u64,
        constants.worker_default_inflight_chunks as u64,
        constants.worker_heartbeat_ms,
        constants.case_concurrency as u64,
        constants.evaluator_concurrency as u64,
        constants.wasm_concurrency as u64,
        constants.wasm_max_memory_mib as u64,
        constants.result_insert_group_size as u64,
    ];
    if values.contains(&0) {
        bail!("registered production constants must be positive");
    }
    Ok(())
}

fn validate_profile_implementation_contract(
    registry: &super::model::WorkloadRegistry,
) -> Result<()> {
    let implemented: Vec<_> = registry
        .workloads
        .iter()
        .filter(|workload| workload.status == ImplementationStatus::Implemented)
        .collect();
    if implemented.len() != 1 || implemented[0].id != "startup.cli-help.v1" {
        bail!("Phase 1 must implement only startup.cli-help.v1");
    }
    let startup = implemented[0];
    if startup.command != ["--help"] || startup.help_signatures.is_empty() {
        bail!("startup workload must use the supported --help boundary");
    }
    Ok(())
}

fn validate_dependency_boundaries(root: &Path) -> Result<()> {
    let output = cargo_output(root, &["metadata", "--locked", "--format-version", "1"])?;
    let metadata: Value = serde_json::from_slice(&output)?;
    let packages = metadata["packages"]
        .as_array()
        .context("cargo metadata packages missing")?;
    let vigilo = packages
        .iter()
        .find(|package| package["name"] == "vigilo")
        .context("vigilo package missing from metadata")?;
    for dependency in vigilo["dependencies"]
        .as_array()
        .context("vigilo dependencies missing")?
    {
        let kind = dependency["kind"].as_str().unwrap_or("normal");
        if kind == "dev" {
            continue;
        }
        let name = dependency["name"].as_str().unwrap_or_default();
        let path = dependency["path"].as_str().unwrap_or_default();
        if name == "xtask" || name.starts_with("criterion") || path.contains("performance") {
            bail!("production dependency graph contains performance dependency {name}");
        }
    }
    let xtask = packages
        .iter()
        .find(|package| package["name"] == "xtask")
        .context("xtask package missing from metadata")?;
    if xtask["dependencies"]
        .as_array()
        .context("xtask dependencies missing")?
        .iter()
        .any(|dependency| dependency["name"] == "vigilo")
    {
        bail!("xtask must not depend on vigilo");
    }
    let tree = cargo_output(
        root,
        &[
            "tree",
            "--locked",
            "-p",
            "vigilo",
            "--edges",
            "normal,build",
        ],
    )?;
    let tree = String::from_utf8_lossy(&tree).to_ascii_lowercase();
    if tree.contains("criterion") || tree.lines().any(|line| line.contains("xtask v")) {
        bail!("release dependency tree contains performance tooling");
    }
    Ok(())
}

fn validate_package_contents(root: &Path) -> Result<()> {
    let output = cargo_output(
        root,
        &[
            "package",
            "-p",
            "vigilo",
            "--list",
            "--locked",
            "--allow-dirty",
        ],
    )?;
    for line in String::from_utf8_lossy(&output).lines() {
        let normalized = line.replace('\\', "/").to_ascii_lowercase();
        if normalized.contains("performance/")
            || normalized.contains("xtask/")
            || normalized.contains("target/perf")
            || normalized.contains("criterion")
        {
            bail!("published Vigilo package leaks performance content: {line}");
        }
    }
    Ok(())
}

fn validate_fixture_tree(root: &Path) -> Result<()> {
    fn visit(path: &Path) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                visit(&entry.path())?;
                continue;
            }
            let metadata = entry.metadata()?;
            if metadata.len() > 1024 * 1024 {
                bail!(
                    "checked-in performance fixture is too large: {}",
                    entry.path().display()
                );
            }
            let content = fs::read_to_string(entry.path())?;
            if content.contains("#[path") && content.contains("vigilo/src") {
                bail!(
                    "performance code imports private production source: {}",
                    entry.path().display()
                );
            }
            if matches!(
                entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str()),
                Some("jsonl" | "log")
            ) {
                bail!(
                    "generated performance artifact is checked in: {}",
                    entry.path().display()
                );
            }
        }
        Ok(())
    }
    visit(&root.join("performance"))
}

fn validate_bootstrap_delta(root: &Path, base: &str) -> Result<()> {
    let mut changed = Vec::new();
    for args in [
        vec!["diff", "--name-only", &format!("{base}...HEAD")],
        vec!["diff", "--name-only"],
        vec!["diff", "--cached", "--name-only"],
    ] {
        let owned: Vec<String> = args.into_iter().map(str::to_owned).collect();
        let borrowed: Vec<&str> = owned.iter().map(String::as_str).collect();
        changed.extend(
            String::from_utf8_lossy(&command_output(root, "git", &borrowed)?)
                .lines()
                .map(str::to_owned),
        );
    }
    changed.sort();
    changed.dedup();
    let status_output = command_output(root, "git", &["status", "--porcelain"])?;
    let status = String::from_utf8_lossy(&status_output);
    changed.extend(
        status
            .lines()
            .filter_map(|line| line.get(3..))
            .map(|path| path.rsplit(" -> ").next().unwrap_or(path).to_owned()),
    );
    changed.sort();
    changed.dedup();
    for path in changed {
        let path = path.replace('\\', "/");
        if path.starts_with("vigilo/src/")
            || path.starts_with("migrations/")
            || path == "vigilo/Cargo.toml"
        {
            bail!("bootstrap harness delta changes production path: {path}");
        }
    }
    Ok(())
}

fn validate_endpoints(endpoints: &[String], marker: Option<&str>) -> Result<()> {
    if endpoints.is_empty() {
        return Ok(());
    }
    let marker = marker.context("--ownership-marker is required with --endpoint")?;
    if !marker.starts_with("vigilo_perf_")
        || !marker
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        bail!("ownership marker must use the vigilo_perf_<id> form");
    }
    for endpoint in endpoints {
        validate_endpoint(endpoint, marker)?;
    }
    Ok(())
}

fn validate_endpoint(endpoint: &str, marker: &str) -> Result<()> {
    let lower = endpoint.to_ascii_lowercase();
    let authority = lower
        .split_once("://")
        .map(|(_, rest)| rest)
        .context("endpoint must include a URL scheme")?
        .split('/')
        .next()
        .unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
        bail!("destructive endpoint is not loopback-isolated: {endpoint}");
    }
    if !endpoint.contains(marker) {
        bail!("endpoint does not contain its ownership marker: {endpoint}");
    }
    Ok(())
}

fn process_self_test() -> Result<()> {
    let executable = std::env::current_exe()?;
    let valid = vec!["__perf-fixture".into(), "--delay-ms".into(), "1".into()];
    let outcome = execute_process(&ProcessSpec {
        program: &executable,
        args: &valid,
        current_dir: None,
        timeout: Duration::from_secs(2),
        stdout_limit: 1024,
        stderr_limit: 1024,
    })?;
    if outcome.exit_code != Some(0) || outcome.timed_out {
        bail!("process self-test valid fixture failed");
    }

    let crash = vec!["__perf-fixture".into(), "--exit-code".into(), "7".into()];
    let outcome = execute_process(&ProcessSpec {
        program: &executable,
        args: &crash,
        current_dir: None,
        timeout: Duration::from_secs(2),
        stdout_limit: 1024,
        stderr_limit: 1024,
    })?;
    if outcome.exit_code != Some(7) || outcome.timed_out {
        bail!("process self-test crash classification failed");
    }

    let timeout = vec!["__perf-fixture".into(), "--delay-ms".into(), "1000".into()];
    let outcome = execute_process(&ProcessSpec {
        program: &executable,
        args: &timeout,
        current_dir: None,
        timeout: Duration::from_millis(20),
        stdout_limit: 1024,
        stderr_limit: 1024,
    })?;
    if !outcome.timed_out {
        bail!("process self-test timeout classification failed");
    }

    let truncation = vec![
        "__perf-fixture".into(),
        "--output-bytes".into(),
        "4096".into(),
    ];
    let outcome = execute_process(&ProcessSpec {
        program: &executable,
        args: &truncation,
        current_dir: None,
        timeout: Duration::from_secs(2),
        stdout_limit: 64,
        stderr_limit: 64,
    })?;
    if !outcome.stdout.truncated || outcome.stdout.data.len() != 64 {
        bail!("process self-test truncation classification failed");
    }
    Ok(())
}

fn cargo_output(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    command_output(root, "cargo", args)
}

fn command_output(root: &Path, program: &str, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("run {program} {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_policy_accepts_only_owned_loopback_resources() {
        let marker = "vigilo_perf_123";
        assert!(validate_endpoint("postgres://localhost/vigilo_perf_123", marker).is_ok());
        assert!(validate_endpoint("amqp://127.0.0.1/vigilo_perf_123", marker).is_ok());
        assert!(validate_endpoint("postgres://db.example.com/vigilo_perf_123", marker).is_err());
        assert!(validate_endpoint("postgres://localhost/development", marker).is_err());
    }
}
