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
    verify_startup_capability(&executable)?;

    let assets = output.join("setup-assets");
    let migrations_source = source.join("migrations");
    let wit_source = source.join("wit");
    copy_tree(&migrations_source, &assets.join("migrations"))?;
    copy_tree(&wit_source, &assets.join("wit"))?;
    let migrations_digest = digest_tree(&migrations_source)?;
    let evaluator_abi_digest = digest_tree(&wit_source)?;
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
        capabilities: vec!["startup.cli-help.v1".into()],
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

fn verify_startup_capability(executable: &Path) -> Result<()> {
    let args = vec!["--help".into()];
    let outcome = execute_process(&ProcessSpec {
        program: executable,
        args: &args,
        current_dir: None,
        timeout: Duration::from_secs(10),
        stdout_limit: 256 * 1024,
        stderr_limit: 256 * 1024,
    })?;
    let stdout = outcome.stdout.text();
    if outcome.timed_out
        || outcome.exit_code != Some(0)
        || !stdout.contains("Usage:")
        || !stdout.contains("Commands:")
    {
        bail!("release binary does not satisfy startup.cli-help.v1");
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
