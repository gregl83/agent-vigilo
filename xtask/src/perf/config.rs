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
    if !id
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("invalid profile ID: {id}");
    }
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
        if workload.timing != "informative" && workload.timing != "calibration" {
            bail!("unsupported timing policy: {}", workload.timing);
        }
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
}
