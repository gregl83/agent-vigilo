//! Built-in evaluator bootstrap and publishing workflows.
//!
//! Project setup treats evaluator crates under the workspace `evaluators/`
//! directory as curated Vigilo evaluators. This module owns discovery, release
//! builds, idempotent registry publishing, and the shared publish path used by
//! CLI commands.

use std::{
    env,
    fs,
    path::{
        Path,
        PathBuf,
    },
    time::Instant,
};

use tokio::process::Command;
use tracing::{
    debug,
    info,
};

use crate::{
    context::Context,
    db::tables::evaluators as evaluator_table,
    manifest::read_manifest,
    models::evaluator::{
        Evaluator,
        EvaluatorDraft,
    },
};

const EVALUATOR_NAMESPACE: &str = "vigilo";
const EVALUATORS_DIR: &str = "evaluators";
const RELEASE_PROFILE: &str = "release";
const WASM_TARGET: &str = "wasm32-wasip2";

/// Summary of project evaluator bootstrap work.
#[derive(Debug, Default)]
pub(crate) struct BootstrapEvaluatorsSummary {
    pub(crate) discovered: usize,
    pub(crate) publishable: usize,
    pub(crate) inserted: usize,
    pub(crate) skipped: usize,
}

/// Result of attempting to publish one evaluator package.
#[derive(Debug)]
pub(crate) enum PublishEvaluatorOutcome {
    Inserted(Evaluator),
    Skipped(Evaluator),
}

impl PublishEvaluatorOutcome {
    fn inserted(&self) -> bool {
        matches!(self, Self::Inserted(_))
    }

    fn skipped(&self) -> bool {
        matches!(self, Self::Skipped(_))
    }

    pub(crate) fn evaluator(&self) -> &Evaluator {
        match self {
            Self::Inserted(evaluator) | Self::Skipped(evaluator) => evaluator,
        }
    }
}

/// Builds and publishes all built-in project evaluators.
pub(crate) async fn bootstrap_project_evaluators(
    context: &Context,
) -> anyhow::Result<BootstrapEvaluatorsSummary> {
    let evaluators_dir = project_evaluators_dir()?;
    bootstrap_project_evaluators_from_dir(context, &evaluators_dir).await
}

async fn bootstrap_project_evaluators_from_dir(
    context: &Context,
    evaluators_dir: &Path,
) -> anyhow::Result<BootstrapEvaluatorsSummary> {
    info!(
        "publishing project evaluators from {}",
        evaluators_dir.display()
    );

    let mut summary = BootstrapEvaluatorsSummary::default();

    for evaluator_path in evaluator_package_dirs(evaluators_dir)? {
        summary.discovered += 1;

        let evaluator_label = evaluator_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unknown>");

        if !evaluator_path.join("Vigilo.toml").is_file() {
            info!(
                "evaluator {}: missing Vigilo.toml, skipping",
                evaluator_label
            );
            summary.skipped += 1;
            continue;
        }

        summary.publishable += 1;
        build_release_evaluator(&evaluator_path).await?;

        let outcome =
            publish_vigilo_evaluator(context, evaluator_path, RELEASE_PROFILE.to_string()).await?;

        if outcome.inserted() {
            summary.inserted += 1;
        } else if outcome.skipped() {
            summary.skipped += 1;
        }
    }

    info!(
        discovered = summary.discovered,
        publishable = summary.publishable,
        inserted = summary.inserted,
        skipped = summary.skipped,
        "published project evaluators"
    );

    Ok(summary)
}

/// Publishes one evaluator package into the built-in `vigilo` namespace.
pub(crate) async fn publish_vigilo_evaluator(
    context: &Context,
    evaluator_path: PathBuf,
    profile: String,
) -> anyhow::Result<PublishEvaluatorOutcome> {
    let component = context
        .wasm()
        .await?
        .prepare_evaluator(evaluator_path, profile)?;

    let draft = EvaluatorDraft {
        namespace: EVALUATOR_NAMESPACE.to_string(),
        name: component.name,
        version: component.version,
        content_hash: component.wasm_hash,
        wasm_bytes: component.wasm_bytes,
        interface_name: component.interface_name,
        interface_version: component.interface_version,
        wit_world: component.wit_world,
        runtime: component.runtime,
        runtime_version: component.runtime_version,
        runtime_fingerprint: component.runtime_fingerprint,
        description: component.description,
        tags: component.tags,
        metadata: component.metadata,
    };

    publish_evaluator_draft(context, draft).await
}

async fn publish_evaluator_draft(
    context: &Context,
    draft: EvaluatorDraft,
) -> anyhow::Result<PublishEvaluatorOutcome> {
    let db = context.db().await?;

    if let Some(existing) =
        evaluator_table::select_evaluator(db, &draft.namespace, &draft.name, &draft.version).await?
    {
        if existing.content_hash == draft.content_hash {
            info!(
                "evaluator {}/{}:{} already exists, skipping",
                existing.namespace, existing.name, existing.version
            );
            return Ok(PublishEvaluatorOutcome::Skipped(existing));
        }

        anyhow::bail!(
            "evaluator {}/{}:{} already exists with a different content hash",
            draft.namespace,
            draft.name,
            draft.version,
        );
    }

    if let Some(existing) =
        evaluator_table::select_evaluator_by_content_hash(db, &draft.namespace, &draft.content_hash)
            .await?
    {
        anyhow::bail!(
            "evaluator content hash already exists as {}/{}:{}, refusing to publish duplicate content as {}/{}:{}",
            existing.namespace,
            existing.name,
            existing.version,
            draft.namespace,
            draft.name,
            draft.version,
        );
    }

    let evaluator = evaluator_table::insert_evaluator(db, &draft).await?;
    info!(
        "successfully published evaluator: {}/{}:{}",
        evaluator.namespace, evaluator.name, evaluator.version,
    );

    Ok(PublishEvaluatorOutcome::Inserted(evaluator))
}

async fn build_release_evaluator(evaluator_path: &Path) -> anyhow::Result<()> {
    let evaluator_label = evaluator_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<unknown>");
    let manifest = read_manifest(&evaluator_path.to_path_buf())?;
    let manifest_path = evaluator_path.join(&manifest.package.manifest);

    info!("evaluator {}: building release wasm", evaluator_label);

    let start = Instant::now();
    let status = Command::new(cargo_binary())
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest_path)
        .arg("--target")
        .arg(WASM_TARGET)
        .arg("--release")
        .status()
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "failed to start evaluator build for {}: {}",
                evaluator_path.display(),
                err
            )
        })?;

    if !status.success() {
        anyhow::bail!(
            "evaluator {} build failed with status {}",
            evaluator_path.display(),
            status,
        );
    }

    info!(
        "evaluator {}: built release wasm in {:?}",
        evaluator_label,
        start.elapsed()
    );

    Ok(())
}

fn evaluator_package_dirs(evaluators_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();

    for entry in fs::read_dir(evaluators_dir).map_err(|err| {
        anyhow::anyhow!(
            "failed to read evaluators directory {}: {}",
            evaluators_dir.display(),
            err
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            debug!("skipping non-directory evaluator entry {}", path.display());
            continue;
        }

        dirs.push(path);
    }

    dirs.sort();
    Ok(dirs)
}

fn project_evaluators_dir() -> anyhow::Result<PathBuf> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| anyhow::anyhow!("vigilo crate directory has no workspace parent"))?
        .to_path_buf();

    Ok(workspace_root.join(EVALUATORS_DIR))
}

fn cargo_binary() -> PathBuf {
    if let Some(cargo) = env::var_os("CARGO") {
        return cargo.into();
    }

    let cargo_exe = if cfg!(windows) { "cargo.exe" } else { "cargo" };

    if let Some(home) = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")) {
        let candidate = PathBuf::from(home)
            .join(".cargo")
            .join("bin")
            .join(cargo_exe);
        if candidate.is_file() {
            return candidate;
        }
    }

    cargo_exe.into()
}
