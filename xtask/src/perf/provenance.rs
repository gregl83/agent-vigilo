//! Frozen campaign-input provenance and run-relative contract loading.
//!
//! A campaign snapshots the validated registry, resolved profile, optional
//! budget policy, and verified build-manifest metadata before measurement. The
//! snapshots make later analysis independent of mutable repository inputs and
//! disposable build directories; executable and setup-asset bytes remain owned
//! by the separate build snapshot.

use std::{
    fs,
    path::{
        Component,
        Path,
        PathBuf,
    },
};

use anyhow::{
    Context,
    Result,
    bail,
};
use serde::{
    Serialize,
    de::DeserializeOwned,
};

use super::{
    artifact::atomic_json,
    config::{
        validate_budget_policy,
        validate_profile,
        validate_profile_registry,
        validate_registry,
    },
    model::{
        BUILD_SCHEMA,
        BudgetPolicy,
        BuildManifest,
        CampaignManifest,
        Profile,
        REPORT_SCHEMA,
        WorkloadRegistry,
    },
};

const REGISTRY_FILE: &str = "provenance/workload-registry.json";
const PROFILE_FILE: &str = "provenance/profile.json";
const BUDGET_POLICY_FILE: &str = "provenance/budget-policy.json";
const BASELINE_MANIFEST_FILE: &str = "provenance/baseline-build-manifest.json";
const CANDIDATE_MANIFEST_FILE: &str = "provenance/candidate-build-manifest.json";

/// Run-relative paths written into a campaign manifest after freezing inputs.
pub struct FrozenPaths {
    pub registry_file: String,
    pub profile_file: String,
    pub budget_policy_file: Option<String>,
    pub baseline_manifest: Option<String>,
    pub candidate_manifest: String,
}

/// Validated campaign inputs loaded exclusively from one retained run directory.
pub struct FrozenInputs {
    pub registry: WorkloadRegistry,
    pub profile: Profile,
    pub budget_policy: Option<BudgetPolicy>,
    pub baseline_manifest: Option<BuildManifest>,
    pub candidate_manifest: BuildManifest,
}

/// Persists the already validated inputs needed to audit and analyze a campaign.
pub fn freeze(
    run_dir: &Path,
    registry: &WorkloadRegistry,
    profile: &Profile,
    budget_policy: Option<&BudgetPolicy>,
    baseline_manifest: Option<&BuildManifest>,
    candidate_manifest: &BuildManifest,
) -> Result<FrozenPaths> {
    write(run_dir, REGISTRY_FILE, registry)?;
    write(run_dir, PROFILE_FILE, profile)?;
    if let Some(policy) = budget_policy {
        write(run_dir, BUDGET_POLICY_FILE, policy)?;
    }
    if let Some(manifest) = baseline_manifest {
        write(run_dir, BASELINE_MANIFEST_FILE, manifest)?;
    }
    write(run_dir, CANDIDATE_MANIFEST_FILE, candidate_manifest)?;

    Ok(FrozenPaths {
        registry_file: REGISTRY_FILE.into(),
        profile_file: PROFILE_FILE.into(),
        budget_policy_file: budget_policy.map(|_| BUDGET_POLICY_FILE.into()),
        baseline_manifest: baseline_manifest.map(|_| BASELINE_MANIFEST_FILE.into()),
        candidate_manifest: CANDIDATE_MANIFEST_FILE.into(),
    })
}

/// Loads and validates every frozen contract referenced by `campaign.json`.
pub fn load(run_dir: &Path, campaign: &CampaignManifest) -> Result<FrozenInputs> {
    if campaign.schema_id != REPORT_SCHEMA {
        bail!("campaign provenance uses an unsupported schema");
    }
    let registry: WorkloadRegistry = read(run_dir, &campaign.registry_file)?;
    validate_registry(&registry).context("validate frozen workload registry")?;
    let profile: Profile = read(run_dir, &campaign.profile_file)?;
    validate_profile(&profile).context("validate frozen profile")?;
    validate_profile_registry(&profile, &registry)
        .context("validate frozen profile against frozen registry")?;
    if profile.id != campaign.profile_id {
        bail!("frozen profile identity does not match campaign");
    }
    let selected = profile
        .workloads
        .iter()
        .map(|workload| format!("{}:{}", workload.id, workload.tuple))
        .collect::<Vec<_>>();
    if selected != campaign.selected_workloads {
        bail!("frozen resolved profile does not match campaign workload selection");
    }

    let budget_policy = campaign
        .budget_policy_file
        .as_deref()
        .map(|path| read::<BudgetPolicy>(run_dir, path))
        .transpose()?;
    match (&profile.budget_reference, &budget_policy) {
        (Some(expected), Some(policy)) => {
            validate_budget_policy(policy).context("validate frozen budget policy")?;
            if &policy.id != expected {
                bail!("frozen budget policy identity does not match profile");
            }
        }
        (None, None) => {}
        _ => bail!("frozen profile and budget policy linkage is incomplete"),
    }

    let baseline_manifest = campaign
        .baseline_manifest
        .as_deref()
        .map(|path| read::<BuildManifest>(run_dir, path))
        .transpose()?;
    let candidate_manifest: BuildManifest = read(run_dir, &campaign.candidate_manifest)?;
    for manifest in baseline_manifest.iter().chain([&candidate_manifest]) {
        if manifest.schema_id != BUILD_SCHEMA {
            bail!("frozen build manifest uses an unsupported schema");
        }
    }
    match (campaign.kind.as_str(), baseline_manifest.is_some()) {
        ("compare", true) | ("run", false) => {}
        _ => bail!("campaign kind and frozen baseline manifest are inconsistent"),
    }

    Ok(FrozenInputs {
        registry,
        profile,
        budget_policy,
        baseline_manifest,
        candidate_manifest,
    })
}

fn write<T: Serialize>(run_dir: &Path, relative: &str, value: &T) -> Result<()> {
    atomic_json(&run_dir.join(relative), value)
}

fn read<T: DeserializeOwned>(run_dir: &Path, relative: &str) -> Result<T> {
    let path = resolve(run_dir, relative)?;
    serde_json::from_slice(&fs::read(&path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))
}

/// Resolves one existing regular file without allowing a campaign path to escape its run.
fn resolve(run_dir: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("campaign provenance path must be a non-empty run-relative file");
    }
    let root = fs::canonicalize(run_dir)
        .with_context(|| format!("resolve run directory {}", run_dir.display()))?;
    let path = fs::canonicalize(run_dir.join(relative))
        .with_context(|| format!("resolve campaign provenance file {relative:?}"))?;
    if !path.starts_with(&root) || !path.is_file() {
        bail!("campaign provenance path escaped its run directory");
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::perf::{
        artifact::no_extra,
        config::{
            load_profile,
            load_registry,
        },
        model::SetupAsset,
    };

    fn manifest(digest: &str) -> BuildManifest {
        BuildManifest {
            schema_id: BUILD_SCHEMA.into(),
            created_at: "2026-08-25T00:00:00Z".into(),
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
            extra: no_extra(),
        }
    }

    fn campaign(paths: FrozenPaths, profile: &Profile, kind: &str) -> CampaignManifest {
        CampaignManifest {
            schema_id: REPORT_SCHEMA.into(),
            run_id: "provenance-test".into(),
            kind: kind.into(),
            status: "pass".into(),
            created_at: "2026-08-25T00:00:00Z".into(),
            completed_at: Some("2026-08-25T00:01:00Z".into()),
            profile_id: profile.id.clone(),
            schedule_seed: 1,
            selected_workloads: profile
                .workloads
                .iter()
                .map(|workload| format!("{}:{}", workload.id, workload.tuple))
                .collect(),
            planned_measured_executions: 0,
            planned_preconditioning_executions: 0,
            artifact_limit_bytes: 1_000_000,
            environment_file: "environment.json".into(),
            registry_file: paths.registry_file,
            profile_file: paths.profile_file,
            budget_policy_file: paths.budget_policy_file,
            baseline_manifest: paths.baseline_manifest,
            candidate_manifest: paths.candidate_manifest,
            failure: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn frozen_inputs_round_trip_and_reject_tampering() {
        let root = crate::perf::artifact::workspace_root().unwrap();
        let registry = load_registry(&root).unwrap();
        let profile = load_profile(&root, "developer-v1").unwrap();
        let baseline = manifest("baseline");
        let candidate = manifest("candidate");
        let directory = tempfile::tempdir().unwrap();
        let paths = freeze(
            directory.path(),
            &registry,
            &profile,
            None,
            Some(&baseline),
            &candidate,
        )
        .unwrap();
        let campaign = campaign(paths, &profile, "compare");

        let loaded = load(directory.path(), &campaign).unwrap();
        assert_eq!(loaded.registry.revision, registry.revision);
        assert_eq!(loaded.profile.id, profile.id);
        assert_eq!(
            loaded.baseline_manifest.unwrap().executable_digest,
            "baseline"
        );
        assert_eq!(loaded.candidate_manifest.executable_digest, "candidate");

        let mut changed: Profile = read(directory.path(), &campaign.profile_file).unwrap();
        changed.id = "other-profile".into();
        write(directory.path(), &campaign.profile_file, &changed).unwrap();
        assert!(load(directory.path(), &campaign).is_err());
    }

    #[test]
    fn provenance_paths_cannot_escape_the_run_directory() {
        let run = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        assert!(resolve(run.path(), "../outside.json").is_err());
        assert!(resolve(run.path(), outside.path().to_string_lossy().as_ref()).is_err());
        assert!(resolve(run.path(), "missing.json").is_err());
    }
}
