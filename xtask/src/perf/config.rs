//! Loading, validation, and resolution of workload registries and profiles.
//!
//! Registry entries define semantic workload contracts; profiles select exact
//! registered tuples and execution budgets. Resolution is strict: unknown,
//! out-of-profile, or unavailable selections fail rather than being skipped.

use std::{
    collections::BTreeSet,
    fs,
    path::Path,
};

use anyhow::{
    Context,
    Result,
    bail,
};

use super::model::{
    BUDGET_SCHEMA,
    BudgetPolicy,
    PROFILE_SCHEMA,
    Profile,
    ProfileWorkload,
    REGISTRY_SCHEMA,
    Workload,
    WorkloadRegistry,
};

/// A profile selection paired with its resolved registry contract.
pub struct SelectedWorkload<'a> {
    /// Profile-specific tuple, block count, and timing policy.
    pub profile: &'a ProfileWorkload,
    /// Registry-owned workload definition and execution requirements.
    pub workload: &'a Workload,
}

/// Loads and validates the workspace's versioned MVP workload registry.
pub fn load_registry(root: &Path) -> Result<WorkloadRegistry> {
    let path = root.join("performance/registry/workloads-v1.toml");
    let registry: WorkloadRegistry = toml::from_str(
        &fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    validate_registry(&registry)?;
    Ok(registry)
}

/// Loads and validates a named profile from `performance/profiles`.
pub fn load_profile(root: &Path, id: &str) -> Result<Profile> {
    validate_file_id(id, "profile")?;
    let path = root
        .join(super::default_profile_dir())
        .join(format!("{id}.toml"));
    let profile: Profile = toml::from_str(
        &fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    validate_profile(&profile)?;
    Ok(profile)
}

/// Loads and validates one reviewed performance-budget policy.
pub fn load_budget_policy(root: &Path, id: &str) -> Result<BudgetPolicy> {
    validate_file_id(id, "budget")?;
    let path = root.join("performance/budgets").join(format!("{id}.toml"));
    let policy: BudgetPolicy = toml::from_str(
        &fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    if policy.id != id {
        bail!("budget file identity {} does not match {id}", policy.id);
    }
    validate_budget_policy(&policy)?;
    Ok(policy)
}

/// Validates registry schema, uniqueness, and required execution metadata.
pub fn validate_registry(registry: &WorkloadRegistry) -> Result<()> {
    if registry.schema_id != REGISTRY_SCHEMA {
        bail!("unsupported registry schema: {}", registry.schema_id);
    }
    if registry.revision == 0 {
        bail!("registry revision must be positive");
    }
    if registry.extra.keys().any(String::is_empty) {
        bail!("registry extension keys cannot be empty");
    }
    let mut ids = BTreeSet::new();
    for workload in &registry.workloads {
        if !ids.insert(&workload.id) {
            bail!("duplicate workload ID: {}", workload.id);
        }
        if workload.tuples.is_empty()
            || workload.watchdog_ms == 0
            || workload.planning_duration_ms == 0
        {
            bail!(
                "workload {} has an incomplete execution contract",
                workload.id
            );
        }
        if workload.owner.is_empty()
            || workload.fixture.is_empty()
            || workload.unit.is_empty()
            || workload.oracle.is_empty()
            || workload.required_metrics.is_empty()
        {
            bail!(
                "workload {} has incomplete ownership or measurement metadata",
                workload.id
            );
        }
        if workload.extra.keys().any(String::is_empty) {
            bail!("workload {} has an empty extension key", workload.id);
        }
    }
    Ok(())
}

/// Validates profile schema, limits, block balance, and timing policies.
pub fn validate_profile(profile: &Profile) -> Result<()> {
    if profile.schema_id != PROFILE_SCHEMA {
        bail!("unsupported profile schema: {}", profile.schema_id);
    }
    if profile.id.is_empty()
        || profile.description.is_empty()
        || profile.workloads.is_empty()
        || profile.campaign_cap_secs == 0
        || profile.max_artifact_bytes == 0
        || profile.max_stdout_bytes == 0
        || profile.max_stderr_bytes == 0
    {
        bail!(
            "profile {} has an incomplete execution contract",
            profile.id
        );
    }
    if profile.extra.keys().any(String::is_empty) {
        bail!("profile {} has an empty extension key", profile.id);
    }
    for workload in &profile.workloads {
        if workload.blocks == 0 || workload.blocks % 2 != 0 {
            bail!(
                "profile {} workload {} must declare a positive even block count",
                profile.id,
                workload.id
            );
        }
        if !matches!(
            workload.timing.as_str(),
            "informative" | "calibration" | "capacity" | "gating"
        ) {
            bail!("unsupported timing policy: {}", workload.timing);
        }
    }
    let gating = profile
        .workloads
        .iter()
        .filter(|workload| workload.timing == "gating")
        .count();
    if gating > 0 && (gating != profile.workloads.len() || profile.budget_reference.is_none()) {
        bail!("gating profiles require one budget policy and cannot mix timing modes");
    }
    if gating == 0 && profile.budget_reference.is_some() {
        bail!("only gating profiles may reference a budget policy");
    }
    if let Some(reference) = profile.budget_reference.as_deref() {
        validate_file_id(reference, "budget")?;
    }
    Ok(())
}

/// Validates reviewed budget identity, uniqueness, and finite positive gates.
pub fn validate_budget_policy(policy: &BudgetPolicy) -> Result<()> {
    if policy.schema_id != BUDGET_SCHEMA
        || policy.id.is_empty()
        || policy.environment_id.is_empty()
        || policy.calibration_id.is_empty()
        || policy.approved_at.is_empty()
        || policy.approved_by.is_empty()
        || policy.entries.is_empty()
    {
        bail!("budget policy has an incompatible or incomplete contract");
    }
    validate_file_id(&policy.id, "budget")?;
    let mut entries = BTreeSet::new();
    for entry in &policy.entries {
        if entry.workload_id.is_empty()
            || entry.tuple_id.is_empty()
            || entry.metric.is_empty()
            || !(entry.practical_budget.is_finite() && entry.practical_budget > 0.0)
            || entry.minimum_blocks == 0
            || !entry.minimum_blocks.is_multiple_of(2)
            || !(entry.max_residual_orientation_effect.is_finite()
                && entry.max_residual_orientation_effect > 0.0)
        {
            bail!("budget policy contains an invalid entry");
        }
        if !entries.insert((&entry.workload_id, &entry.tuple_id, &entry.metric)) {
            bail!("budget policy contains a duplicate workload metric");
        }
    }
    Ok(())
}

fn validate_file_id(id: &str, kind: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("invalid {kind} ID: {id}");
    }
    Ok(())
}

/// Proves that every profile selection names a registered workload tuple.
pub fn validate_profile_registry(profile: &Profile, registry: &WorkloadRegistry) -> Result<()> {
    for profile_workload in &profile.workloads {
        let workload = registry
            .workloads
            .iter()
            .find(|workload| workload.id == profile_workload.id)
            .with_context(|| {
                format!(
                    "profile {} references unknown workload {}",
                    profile.id, profile_workload.id
                )
            })?;
        if !workload.tuples.contains(&profile_workload.tuple) {
            bail!(
                "profile {} references unknown tuple {} for {}",
                profile.id,
                profile_workload.tuple,
                workload.id
            );
        }
    }
    Ok(())
}

/// Resolves optional workload filters against an already validated profile.
pub fn select_workloads<'a>(
    profile: &'a Profile,
    registry: &'a WorkloadRegistry,
    requested: &[String],
) -> Result<Vec<SelectedWorkload<'a>>> {
    validate_profile_registry(profile, registry)?;
    if profile.requires_workload_selection && requested.is_empty() {
        bail!("profile {} requires at least one --workload", profile.id);
    }
    let requested: BTreeSet<_> = requested.iter().collect();
    for id in &requested {
        if !profile
            .workloads
            .iter()
            .any(|workload| workload.id == id.as_str())
        {
            bail!("workload {id} is not in profile {}", profile.id);
        }
    }
    let mut selected = Vec::new();
    for profile_workload in &profile.workloads {
        if !requested.is_empty() && !requested.contains(&profile_workload.id) {
            continue;
        }
        let workload = registry
            .workloads
            .iter()
            .find(|workload| workload.id == profile_workload.id)
            .expect("profile registry validation must resolve workload");
        selected.push(SelectedWorkload {
            profile: profile_workload,
            workload,
        });
    }
    Ok(selected)
}

/// Estimates comparison campaign time from declared planning durations.
pub fn estimated_compare_millis(selected: &[SelectedWorkload<'_>]) -> u64 {
    selected
        .iter()
        .map(|selected| {
            let measured =
                u64::from(selected.profile.blocks) * 4 * selected.workload.planning_duration_ms;
            let precondition = if selected.workload.preconditioning
                == super::model::Preconditioning::OnePerBinary
            {
                2 * selected.workload.planning_duration_ms
            } else {
                0
            };
            measured + precondition
        })
        .sum()
}

/// Estimates single-binary campaign time from declared planning durations.
pub fn estimated_single_millis(selected: &[SelectedWorkload<'_>]) -> u64 {
    selected
        .iter()
        .map(|selected| {
            let measured =
                u64::from(selected.profile.blocks) * 2 * selected.workload.planning_duration_ms;
            let precondition = if selected.workload.preconditioning
                == super::model::Preconditioning::OnePerBinary
            {
                selected.workload.planning_duration_ms
            } else {
                0
            };
            measured + precondition
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_block_count_is_rejected() {
        let profile: Profile = toml::from_str(
            r#"
schema_id = "profile/v1"
id = "bad-v1"
description = "bad"
requires_workload_selection = false
campaign_cap_secs = 1
schedule_seed = 1
max_artifact_bytes = 1
max_stdout_bytes = 1
max_stderr_bytes = 1
[[workloads]]
id = "startup.cli-help.v1"
tuple = "cold-help"
blocks = 3
timing = "informative"
"#,
        )
        .unwrap();
        assert!(validate_profile(&profile).is_err());
    }

    #[test]
    fn repository_profiles_resolve_strict_selections_and_estimates() {
        let root = crate::perf::artifact::workspace_root().unwrap();
        let registry = load_registry(&root).unwrap();
        let developer = load_profile(&root, "developer-v1").unwrap();
        assert!(load_profile(&root, "../escape").is_err());
        assert!(select_workloads(&developer, &registry, &[]).is_err());
        assert!(select_workloads(&developer, &registry, &["run.create.v1".into()]).is_err());

        let selected =
            select_workloads(&developer, &registry, &["startup.cli-help.v1".into()]).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(estimated_single_millis(&selected), 5_000);
        assert_eq!(estimated_compare_millis(&selected), 10_000);

        let pr = load_profile(&root, "pr-v1").unwrap();
        assert_eq!(select_workloads(&pr, &registry, &[]).unwrap().len(), 4);

        let capacity = load_profile(&root, "capacity-v1").unwrap();
        assert_eq!(
            select_workloads(&capacity, &registry, &[]).unwrap().len(),
            10
        );
    }

    #[test]
    fn registry_and_profile_validation_reject_contract_drift() {
        let root = crate::perf::artifact::workspace_root().unwrap();
        let registry = load_registry(&root).unwrap();
        let profile = load_profile(&root, "pr-v1").unwrap();

        let mut invalid = registry.clone();
        invalid.schema_id = "workload-registry/v2".into();
        assert!(validate_registry(&invalid).is_err());
        let mut invalid = registry.clone();
        invalid.revision = 0;
        assert!(validate_registry(&invalid).is_err());
        let mut invalid = registry.clone();
        invalid.workloads.push(invalid.workloads[0].clone());
        assert!(validate_registry(&invalid).is_err());
        let mut invalid = registry.clone();
        invalid.workloads[0].tuples.clear();
        assert!(validate_registry(&invalid).is_err());
        let mut invalid = registry.clone();
        invalid.workloads[0].owner.clear();
        assert!(validate_registry(&invalid).is_err());

        let mut invalid = profile.clone();
        invalid.schema_id = "profile/v2".into();
        assert!(validate_profile(&invalid).is_err());
        let mut invalid = profile.clone();
        invalid.max_stdout_bytes = 0;
        assert!(validate_profile(&invalid).is_err());
        let mut invalid = profile.clone();
        invalid.workloads[0].timing = "unknown".into();
        assert!(validate_profile(&invalid).is_err());
        let mut invalid = profile.clone();
        invalid.workloads[0].id = "unknown.v1".into();
        assert!(validate_profile_registry(&invalid, &registry).is_err());
        let mut invalid = profile;
        invalid.workloads[0].tuple = "unknown".into();
        assert!(validate_profile_registry(&invalid, &registry).is_err());
    }

    #[test]
    fn gating_profiles_and_budget_policies_fail_closed() {
        let root = crate::perf::artifact::workspace_root().unwrap();
        let mut profile = load_profile(&root, "developer-v1").unwrap();
        profile.workloads[0].timing = "gating".into();
        assert!(validate_profile(&profile).is_err());
        profile.budget_reference = Some("reference-v2-budgets".into());
        assert!(validate_profile(&profile).is_ok());

        let mut policy: super::super::model::BudgetPolicy = toml::from_str(
            r#"
schema_id = "performance-budget/v1"
id = "reference-v2-budgets"
environment_id = "aws-m6i-2xlarge-al2023-v1"
calibration_id = "calibration-1"
approved_at = "2026-08-21T00:00:00Z"
approved_by = "reviewer"
[[entries]]
workload_id = "startup.cli-help.v1"
tuple_id = "cold-help"
metric = "wall_time"
practical_budget = 0.05
minimum_blocks = 6
max_residual_orientation_effect = 0.02
"#,
        )
        .unwrap();
        assert!(validate_budget_policy(&policy).is_ok());
        policy.entries.push(policy.entries[0].clone());
        assert!(validate_budget_policy(&policy).is_err());
        policy.entries.pop();
        policy.entries[0].practical_budget = 0.0;
        assert!(validate_budget_policy(&policy).is_err());
    }
}
