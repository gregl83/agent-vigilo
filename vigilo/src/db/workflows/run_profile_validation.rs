//! Run profile executability validation.
//!
//! This workflow checks that a proposed run profile can execute against a
//! dataset before run creation persists any work. It validates evaluator refs,
//! published evaluator state, and case-to-group matching so failures are caught
//! before chunks are dispatched.

use std::collections::{
    HashMap,
    HashSet,
};

use sqlx::PgPool;

use crate::{
    agent_client,
    contracts::{
        evaluator_ref::{
            EvaluatorIdentity,
            parse_fully_qualified_evaluator,
        },
        run::{
            DatasetCase,
            RunDataset,
            RunProfile,
        },
    },
    db::tables::evaluators,
    models::evaluator::EvaluatorState,
};

/// Counts produced by a successful profile executability check.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ProfileExecutabilitySummary {
    pub(crate) expected_execution_count: usize,
    pub(crate) expected_evaluator_execution_count: usize,
    pub(crate) unique_evaluator_ref_count: usize,
    pub(crate) runnable_evaluator_ref_count: usize,
}

/// Validates that a profile can execute every dataset case with runnable evaluators.
///
/// The function reports all discovered profile/dataset issues in one error so
/// callers can fix invalid refs, missing evaluators, and unmatched cases
/// together.
///
/// Query behavior:
/// - Parse and de-duplicate evaluator refs from all case groups.
/// - Fetch evaluator runtime metadata for those identities once.
/// - Validate that every matched case has at least one runnable published
///   evaluator before run creation writes durable work.
///
/// The rest of the checks are in-memory profile/dataset validation so callers
/// get a complete list of issues without partial database writes.
pub(crate) async fn validate_profile_executability(
    db: &PgPool,
    profile: &RunProfile,
    dataset: &RunDataset,
) -> anyhow::Result<ProfileExecutabilitySummary> {
    let mut runnable_by_ref: HashMap<String, bool> = HashMap::new();
    let mut parsed_refs: Vec<(String, String, EvaluatorIdentity)> = Vec::new();
    let mut issues = collect_static_profile_config_issues(profile);

    for group in &profile.case_groups {
        for binding in &group.evaluators {
            if runnable_by_ref.contains_key(&binding.evaluator_ref) {
                continue;
            }

            let identity = match parse_fully_qualified_evaluator(&binding.evaluator_ref) {
                Ok(identity) => identity,
                Err(err) => {
                    issues.push(format!(
                        "case_group '{}' uses invalid evaluator ref '{}': {}",
                        group.id, binding.evaluator_ref, err
                    ));
                    runnable_by_ref.insert(binding.evaluator_ref.clone(), false);
                    continue;
                }
            };

            runnable_by_ref.insert(binding.evaluator_ref.clone(), false);
            parsed_refs.push((group.id.clone(), binding.evaluator_ref.clone(), identity));
        }
    }

    let identities = parsed_refs
        .iter()
        .map(|(_, _, identity)| {
            (
                identity.namespace.clone(),
                identity.name.clone(),
                identity.version.clone(),
            )
        })
        .collect::<Vec<_>>();
    let fetched =
        evaluators::select_evaluator_runtime_metadata_by_identities(db, &identities).await?;
    let fetched_by_identity = fetched
        .into_iter()
        .map(|row| {
            let key = (row.namespace.clone(), row.name.clone(), row.version.clone());
            (key, row)
        })
        .collect::<HashMap<_, _>>();

    for (group_id, evaluator_ref, identity) in parsed_refs {
        let key = (
            identity.namespace.clone(),
            identity.name.clone(),
            identity.version.clone(),
        );

        let Some(evaluator_record) = fetched_by_identity.get(&key) else {
            issues.push(format!(
                "case_group '{}' references unpublished evaluator '{}'; publish it before run create/test",
                group_id, evaluator_ref
            ));
            continue;
        };

        if !is_runnable_state(&evaluator_record.state) {
            issues.push(format!(
                "case_group '{}' references evaluator '{}' in non-runnable state '{:?}'",
                group_id, evaluator_ref, evaluator_record.state
            ));
            continue;
        }

        runnable_by_ref.insert(evaluator_ref, true);
    }

    let mut expected_evaluator_execution_count = 0usize;

    for case in &dataset.cases {
        let matching_groups = matching_groups_for_case(profile, case);

        if matching_groups.is_empty() {
            issues.push(format!(
                "case '{}' did not match any case_group (task_type='{}')",
                case.id, case.task_type
            ));
            continue;
        }

        let runnable_for_case = matching_groups
            .iter()
            .flat_map(|group| group.evaluators.iter())
            .filter(|binding| {
                runnable_by_ref
                    .get(&binding.evaluator_ref)
                    .copied()
                    .unwrap_or(false)
            })
            .count();

        if runnable_for_case == 0 {
            let group_ids = matching_groups
                .iter()
                .map(|group| group.id.clone())
                .collect::<Vec<_>>()
                .join(",");
            issues.push(format!(
                "case '{}' matched case_groups [{}] but none had runnable published evaluators",
                case.id, group_ids
            ));
            continue;
        }

        expected_evaluator_execution_count += runnable_for_case;
    }

    if !issues.is_empty() {
        anyhow::bail!(
            "profile executability validation failed:\n- {}",
            issues.join("\n- ")
        );
    }

    let runnable_evaluator_ref_count = runnable_by_ref.values().filter(|is_ok| **is_ok).count();

    Ok(ProfileExecutabilitySummary {
        expected_execution_count: dataset.cases.len(),
        expected_evaluator_execution_count,
        unique_evaluator_ref_count: runnable_by_ref.len(),
        runnable_evaluator_ref_count,
    })
}

fn collect_static_profile_config_issues(profile: &RunProfile) -> Vec<String> {
    let mut issues = Vec::new();

    if profile.agent.provider.trim().is_empty() {
        issues.push("agent.provider must not be empty".to_string());
    }

    if profile.agent.name.trim().is_empty() {
        issues.push("agent.name must not be empty".to_string());
    }

    if let Err(err) = reqwest::Url::parse(&profile.agent.http.url) {
        issues.push(format!("agent.http.url is invalid: {}", err));
    }

    if let Err(err) = profile.agent.http.method.parse::<reqwest::Method>() {
        issues.push(format!(
            "agent.http.method '{}' is invalid: {}",
            profile.agent.http.method, err
        ));
    }

    if profile.agent.http.timeout_secs == Some(0) {
        issues.push("agent.http.timeout_secs must be greater than zero".to_string());
    }

    if profile.defaults.max_attempts == 0 {
        issues.push("defaults.max_attempts must be greater than zero".to_string());
    }

    if !profile.defaults.min_execution_score.is_finite()
        || !(0.0..=1.0).contains(&profile.defaults.min_execution_score)
    {
        issues.push(
            "defaults.min_execution_score must be finite and between 0.0 and 1.0".to_string(),
        );
    }

    if let Err(err) = agent_client::validate_request_format(profile) {
        issues.push(err.to_string());
    }

    for group in &profile.case_groups {
        let mut evaluator_refs = HashSet::new();
        for binding in &group.evaluators {
            if !binding.weight.is_finite() || binding.weight < 0.0 {
                issues.push(format!(
                    "case_group '{}' evaluator '{}' weight must be finite and greater than or equal to 0.0",
                    group.id, binding.evaluator_ref
                ));
            }

            if !evaluator_refs.insert(binding.evaluator_ref.clone()) {
                issues.push(format!(
                    "case_group '{}' contains duplicate evaluator ref '{}'; split it into a separate versioned evaluator or case_group",
                    group.id, binding.evaluator_ref
                ));
            }
        }

        for (dimension, policy) in &group.aggregation.dimensions {
            if !policy.weight.is_finite() || policy.weight < 0.0 {
                issues.push(format!(
                    "case_group '{}' aggregation dimension '{}' weight must be finite and greater than or equal to 0.0",
                    group.id, dimension
                ));
            }
        }
    }

    issues
}

/// Returns the case groups that should evaluate one dataset case.
///
/// Database behavior: none. Explicit `case_group` ids bypass task/tag matching;
/// otherwise the group predicate requires task type, optional any-tag, and all
/// required tags.
fn matching_groups_for_case<'a>(
    profile: &'a RunProfile,
    case: &DatasetCase,
) -> Vec<&'a crate::contracts::run::CaseGroupProfile> {
    match case.case_group.as_deref() {
        Some(group_id) => profile
            .case_groups
            .iter()
            .filter(|group| group.id == group_id)
            .collect(),
        None => profile
            .case_groups
            .iter()
            .filter(|group| {
                if group.applies_to.task_type != case.task_type {
                    return false;
                }

                if !group.applies_to.tags_any.is_empty()
                    && !group
                        .applies_to
                        .tags_any
                        .iter()
                        .any(|tag| case.tags.iter().any(|case_tag| case_tag == tag))
                {
                    return false;
                }

                if !group
                    .applies_to
                    .tags_all
                    .iter()
                    .all(|tag| case.tags.iter().any(|case_tag| case_tag == tag))
                {
                    return false;
                }

                true
            })
            .collect(),
    }
}

/// Returns whether a persisted evaluator state may be used by workers.
///
/// Database behavior: none. The same rule is used by validation and worker-side
/// runtime loading so a run cannot start with an evaluator that workers reject.
fn is_runnable_state(state: &EvaluatorState) -> bool {
    matches!(
        state,
        EvaluatorState::Active | EvaluatorState::Deprecated | EvaluatorState::Yanked
    )
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::{
        contracts::run::{
            AgentHttpConfig,
            AgentProfile,
            AggregationMethod,
            AggregationSettings,
            AppliesTo,
            CaseGroupProfile,
            DatasetCase,
            DimensionAggregation,
            EvaluatorBinding,
            PersistRawOutputsMode,
            PersistenceMode,
            PersistenceSettings,
            RunDefaults,
            RunProfile,
        },
        db::workflows::run_profile_validation::matching_groups_for_case,
    };

    fn profile() -> RunProfile {
        RunProfile {
            profile_id: "p".to_string(),
            profile_version: "1".to_string(),
            description: "d".to_string(),
            defaults: RunDefaults {
                max_attempts: 1,
                request_timeout_secs: 10,
                fail_on_any_blocking_failure: true,
                min_execution_score: 0.5,
            },
            persistence: PersistenceSettings {
                mode: PersistenceMode::Full,
                persist_raw_outputs: PersistRawOutputsMode::All,
                persist_evaluator_evidence: true,
            },
            agent: AgentProfile {
                provider: "example".to_string(),
                name: "test-agent".to_string(),
                version: Some("1.0.0".to_string()),
                model: Some("test-model".to_string()),
                prompt_config_id: Some("test-prompt".to_string()),
                prompt_config_version: Some("1.0.0".to_string()),
                http: AgentHttpConfig {
                    url: "http://127.0.0.1:8787/v1/agent/invoke".to_string(),
                    method: "POST".to_string(),
                    headers: Default::default(),
                    timeout_secs: Some(30),
                },
                config: serde_json::json!({}),
            },
            case_groups: vec![CaseGroupProfile {
                id: "classification".to_string(),
                description: "x".to_string(),
                applies_to: AppliesTo {
                    task_type: "classification".to_string(),
                    tags_any: vec!["smoke".to_string()],
                    tags_all: vec![],
                },
                evaluators: vec![],
                aggregation: AggregationSettings {
                    dimensions: Default::default(),
                },
            }],
        }
    }

    #[test]
    fn matching_groups_use_case_group_override_when_present() {
        let profile = profile();
        let case = DatasetCase {
            id: Uuid::parse_str("018f1111-1111-7111-8111-111111111201").unwrap(),
            task_type: "different".to_string(),
            case_group: Some("classification".to_string()),
            input: serde_json::Value::Null,
            expected: None,
            context: None,
            tags: vec![],
            metadata: Default::default(),
        };

        let groups = matching_groups_for_case(&profile, &case);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, "classification");
    }

    #[test]
    fn matching_groups_apply_task_type_and_tags() {
        let profile = profile();
        let case = DatasetCase {
            id: Uuid::parse_str("018f1111-1111-7111-8111-111111111202").unwrap(),
            task_type: "classification".to_string(),
            case_group: None,
            input: serde_json::Value::Null,
            expected: None,
            context: None,
            tags: vec!["smoke".to_string()],
            metadata: Default::default(),
        };

        let groups = matching_groups_for_case(&profile, &case);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, "classification");
    }

    #[test]
    fn profile_rejects_out_of_range_min_execution_score() {
        let mut profile = profile();
        profile.defaults.min_execution_score = 1.1;

        let issues = super::collect_static_profile_config_issues(&profile);

        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("min_execution_score"))
        );
    }

    #[test]
    fn profile_rejects_negative_evaluator_weight() {
        let mut profile = profile();
        profile.case_groups[0].evaluators.push(EvaluatorBinding {
            evaluator_ref: "core/json-schema:1.0.0".to_string(),
            dimension: "format".to_string(),
            blocking: true,
            weight: -1.0,
            config: serde_json::json!({}),
        });

        let issues = super::collect_static_profile_config_issues(&profile);

        assert!(issues.iter().any(|issue| issue.contains("weight")));
    }

    #[test]
    fn profile_rejects_negative_dimension_weight() {
        let mut profile = profile();
        profile.case_groups[0].aggregation.dimensions.insert(
            "quality".to_string(),
            DimensionAggregation {
                method: AggregationMethod::WeightedMean,
                blocking: false,
                weight: -1.0,
            },
        );

        let issues = super::collect_static_profile_config_issues(&profile);

        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("dimension 'quality' weight"))
        );
    }

    #[test]
    fn profile_rejects_duplicate_evaluator_ref_within_group() {
        let mut profile = profile();
        profile.case_groups[0].evaluators.push(EvaluatorBinding {
            evaluator_ref: "core/json-schema:1.0.0".to_string(),
            dimension: "format".to_string(),
            blocking: true,
            weight: 1.0,
            config: serde_json::json!({}),
        });
        profile.case_groups[0].evaluators.push(EvaluatorBinding {
            evaluator_ref: "core/json-schema:1.0.0".to_string(),
            dimension: "quality".to_string(),
            blocking: false,
            weight: 1.0,
            config: serde_json::json!({}),
        });

        let issues = super::collect_static_profile_config_issues(&profile);

        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("duplicate evaluator ref"))
        );
    }
}
