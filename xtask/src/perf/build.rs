//! Immutable release build snapshots and provenance manifests.
//!
//! Builds run outside measured campaigns. Each snapshot records the executable,
//! toolchain and dependency provenance, migration and evaluator ABI digests, and
//! the semantic workload capabilities that the resulting binary can execute.

use std::{
    collections::BTreeMap,
    fs,
    path::{
        Path,
        PathBuf,
    },
    process::Command,
    time::Duration,
};

use anyhow::{
    Context,
    Result,
    bail,
};
use chrono::Utc;
use clap::Args;

use super::{
    EXIT_PASS,
    artifact::{
        CampaignLease,
        atomic_json,
        copy_tree,
        digest_file,
        digest_tree,
        require_artifact_subpath,
        workspace_root,
    },
    model::{
        BUILD_SCHEMA,
        BuildManifest,
        SetupAsset,
    },
    process::{
        ProcessSpec,
        execute as execute_process,
    },
};

/// CLI arguments for creating a release build snapshot.
#[derive(Debug, Args)]
pub struct BuildArgs {
    /// Source worktree containing the Vigilo workspace.
    #[arg(long, default_value = ".")]
    source: PathBuf,
    /// Unique target directory under this workspace's target/perf/builds.
    #[arg(long)]
    output: PathBuf,
    /// Replace an existing harness-owned build directory.
    #[arg(long)]
    force: bool,
}

/// Builds Vigilo and writes its immutable build manifest and setup assets.
pub fn execute(args: BuildArgs) -> Result<u8> {
    let harness_root = workspace_root()?;
    let source = fs::canonicalize(&args.source)
        .with_context(|| format!("resolve source {}", args.source.display()))?;
    if !source.join("Cargo.toml").is_file() || !source.join("vigilo/Cargo.toml").is_file() {
        bail!("source is not a Vigilo workspace: {}", source.display());
    }
    let output = require_artifact_subpath(&harness_root, &args.output, "builds")?;
    let _lease = CampaignLease::acquire(&harness_root)?;
    if output.exists() {
        if !args.force {
            bail!(
                "build directory already exists; choose a unique --output or pass --force: {}",
                output.display()
            );
        }
        fs::remove_dir_all(&output)
            .with_context(|| format!("remove harness build {}", output.display()))?;
    }
    fs::create_dir_all(&output)?;

    println!("Building release binary from {}", source.display());
    let status = Command::new("cargo")
        .args([
            "build",
            "--locked",
            "--release",
            "-p",
            "vigilo",
            "--target-dir",
        ])
        .arg(&output)
        .current_dir(&source)
        .status()
        .context("run nested cargo build --locked")?;
    if !status.success() {
        bail!("release build failed with {status}");
    }

    let executable_name = if cfg!(windows) {
        "vigilo.exe"
    } else {
        "vigilo"
    };
    let executable = output.join("release").join(executable_name);
    if !executable.is_file() {
        bail!("release build did not produce {}", executable.display());
    }
    let capabilities = verify_capabilities(&executable)?;

    let assets = output.join("setup-assets");
    let migrations_source = source.join("migrations");
    let wit_source = source.join("wit");
    copy_tree(&migrations_source, &assets.join("migrations"))?;
    copy_tree(&wit_source, &assets.join("wit"))?;
    let evaluator_source = source.join("evaluators/sentiment-basic-en");
    let evaluator_target = output.join("evaluator-target");
    let evaluator_status = Command::new("cargo")
        .args(["build", "--locked", "--release", "--manifest-path"])
        .arg(evaluator_source.join("Cargo.toml"))
        .args(["--target", "wasm32-wasip2", "--target-dir"])
        .arg(&evaluator_target)
        .current_dir(&source)
        .status()
        .context("build frozen evaluator fixture")?;
    if !evaluator_status.success() {
        bail!("frozen evaluator fixture build failed with {evaluator_status}");
    }
    let evaluator_asset = assets.join("evaluators/sentiment-basic-en");
    copy_tree(&evaluator_source, &evaluator_asset)?;
    let evaluator_wasm = evaluator_target.join("wasm32-wasip2/release/sentiment_basic_en.wasm");
    let evaluator_asset_wasm =
        evaluator_asset.join("target/wasm32-wasip2/release/sentiment_basic_en.wasm");
    fs::create_dir_all(
        evaluator_asset_wasm
            .parent()
            .context("evaluator asset parent")?,
    )?;
    fs::copy(&evaluator_wasm, &evaluator_asset_wasm).with_context(|| {
        format!(
            "copy evaluator fixture {} to {}",
            evaluator_wasm.display(),
            evaluator_asset_wasm.display()
        )
    })?;
    let migrations_digest = digest_tree(&migrations_source)?;
    let evaluator_abi_digest = digest_tree(&wit_source)?;
    let evaluator_fixture_digest = digest_tree(&evaluator_asset)?;
    if migrations_digest != digest_tree(&assets.join("migrations"))?
        || evaluator_abi_digest != digest_tree(&assets.join("wit"))?
    {
        bail!("setup asset snapshot verification failed");
    }

    let cargo_lock = source.join("Cargo.lock");
    let dependency_tree = command_output(
        &source,
        "cargo",
        &[
            "tree",
            "--locked",
            "-p",
            "vigilo",
            "--edges",
            "normal,build",
        ],
    )?;
    let rustc = command_output(&source, "rustc", &["-vV"])?;
    let cargo = command_output(&source, "cargo", &["-V"])?;
    let target = rustc
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or("unknown")
        .to_owned();
    let source_commit = optional_command_output(&source, "git", &["rev-parse", "HEAD"]);
    let source_dirty = optional_command_output(&source, "git", &["status", "--porcelain"])
        .is_some_and(|output| !output.trim().is_empty());

    let manifest = BuildManifest {
        schema_id: BUILD_SCHEMA.into(),
        created_at: Utc::now().to_rfc3339(),
        executable_name: executable_name.into(),
        executable_digest: digest_file(&executable)?,
        executable_bytes: executable.metadata()?.len(),
        source_commit,
        source_dirty,
        source_label: source.display().to_string(),
        cargo_lock_digest: digest_file(&cargo_lock)?,
        dependency_tree_digest: blake3::hash(dependency_tree.as_bytes())
            .to_hex()
            .to_string(),
        migrations_digest: migrations_digest.clone(),
        evaluator_abi_digest: evaluator_abi_digest.clone(),
        rustc,
        cargo,
        target,
        profile: "release".into(),
        capabilities,
        setup_assets: vec![
            SetupAsset {
                name: "migrations".into(),
                relative_path: "setup-assets/migrations".into(),
                digest: migrations_digest,
            },
            SetupAsset {
                name: "evaluator-wit".into(),
                relative_path: "setup-assets/wit".into(),
                digest: evaluator_abi_digest,
            },
            SetupAsset {
                name: "evaluator-fixture".into(),
                relative_path: "setup-assets/evaluators/sentiment-basic-en".into(),
                digest: evaluator_fixture_digest,
            },
        ],
        extra: BTreeMap::new(),
    };
    let manifest_path = output.join("build-manifest.json");
    atomic_json(&manifest_path, &manifest)?;
    println!("Build manifest: {}", manifest_path.display());
    println!("Executable:     {}", executable.display());
    println!("Digest:         {}", manifest.executable_digest);
    Ok(EXIT_PASS)
}

fn verify_capabilities(executable: &Path) -> Result<Vec<String>> {
    verify_help(executable, &["--help"], &["Usage:", "Commands:"])?;
    verify_help(
        executable,
        &["run", "create", "--help"],
        &["profile-file", "dataset-file"],
    )?;
    verify_help(executable, &["coordinator", "once", "--help"], &["Usage:"])?;
    verify_help(executable, &["worker", "once", "--help"], &["Usage:"])?;
    Ok(vec![
        "startup.cli-help.v1".into(),
        "run.create.v1".into(),
        "coordinator.dispatch.v1".into(),
        "worker.execute-wasm.v1".into(),
        "system.lifecycle.v1".into(),
    ])
}

fn verify_help(executable: &Path, arguments: &[&str], signatures: &[&str]) -> Result<()> {
    let args = arguments
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    let outcome = execute_process(&ProcessSpec {
        program: executable,
        args: &args,
        current_dir: None,
        env: &[],
        timeout: Duration::from_secs(10),
        stdout_limit: 256 * 1024,
        stderr_limit: 256 * 1024,
    })?;
    let stdout = outcome.stdout.text();
    if outcome.timed_out
        || outcome.exit_code != Some(0)
        || signatures
            .iter()
            .any(|signature| !stdout.contains(signature))
    {
        bail!("release binary does not satisfy `{}`", arguments.join(" "));
    }
    Ok(())
}

fn command_output(source: &Path, program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(source)
        .output()
        .with_context(|| format!("run {program} {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("{program} {} failed with {}", args.join(" "), output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn optional_command_output(source: &Path, program: &str, args: &[&str]) -> Option<String> {
    command_output(source, program, args).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn help_fixture() -> (PathBuf, Vec<&'static str>) {
        if cfg!(windows) {
            (
                PathBuf::from("powershell.exe"),
                vec!["-NoProfile", "-Command", "Write-Output 'Usage: Commands:'"],
            )
        } else {
            (
                PathBuf::from("/bin/sh"),
                vec!["-c", "printf 'Usage: Commands:\\n'"],
            )
        }
    }

    #[test]
    fn help_probe_requires_success_and_every_signature() {
        let (program, arguments) = help_fixture();
        assert!(verify_help(&program, &arguments, &["Usage:", "Commands:"]).is_ok());
        assert!(verify_help(&program, &arguments, &["missing"]).is_err());
    }

    #[test]
    fn command_probes_capture_required_and_optional_tools() {
        let root = crate::perf::artifact::workspace_root().unwrap();
        assert!(
            command_output(&root, "rustc", &["-V"])
                .unwrap()
                .starts_with("rustc ")
        );
        assert!(optional_command_output(&root, "missing-vigilo-test-tool", &[]).is_none());
    }
}
