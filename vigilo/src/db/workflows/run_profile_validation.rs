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
            NormalizationPolicy,
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

        if evaluator_record.interface_version.as_deref() != Some("1.0.0") {
            issues.push(format!(
                "case_group '{}' references evaluator '{}' with interface version {:?}; expected 1.0.0",
                group_id, evaluator_ref, evaluator_record.interface_version
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
        let mut binding_ids = HashSet::new();
        for binding in &group.evaluators {
            if binding.id.trim().is_empty()
                || !binding
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            {
                issues.push(format!(
                    "case_group '{}' evaluator binding id '{}' must contain only ASCII letters, numbers, '.', '_' or '-'",
                    group.id, binding.id
                ));
            }
            if !binding_ids.insert(binding.id.clone()) {
                issues.push(format!(
                    "case_group '{}' contains duplicate evaluator binding id '{}'",
                    group.id, binding.id
                ));
            }
            if !binding.weight.is_finite() || binding.weight < 0.0 {
                issues.push(format!(
                    "case_group '{}' evaluator '{}' weight must be finite and greater than or equal to 0.0",
                    group.id, binding.evaluator_ref
                ));
            }

            if binding.dimension.trim().is_empty() {
                issues.push(format!(
                    "case_group '{}' evaluator binding '{}' dimension must not be empty",
                    group.id, binding.id
                ));
            }

            if !binding.pass_threshold.is_finite() || !(0.0..=1.0).contains(&binding.pass_threshold)
            {
                issues.push(format!(
                    "case_group '{}' evaluator binding '{}' pass_threshold must be finite and between 0.0 and 1.0",
                    group.id, binding.id
                ));
            }

            if let NormalizationPolicy::Preference {
                preferred,
                tie,
                not_preferred,
            } = &binding.normalization
                && [preferred, tie, not_preferred]
                    .iter()
                    .any(|value| !value.is_finite() || !(0.0..=1.0).contains(*value))
            {
                issues.push(format!(
                    "case_group '{}' evaluator binding '{}' preference normalization values must be finite and between 0.0 and 1.0",
                    group.id, binding.id
                ));
            }

            if !binding.required && (binding.blocking || binding.weight != 0.0) {
                issues.push(format!(
                    "case_group '{}' optional evaluator '{}' must be non-blocking with weight 0.0",
                    group.id, binding.evaluator_ref
                ));
            }
        }

        if !group.evaluators.is_empty() && !group.evaluators.iter().any(|binding| binding.required)
        {
            issues.push(format!(
                "case_group '{}' must contain at least one required evaluator",
                group.id
            ));
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
            NormalizationPolicy,
            PersistRawOutputsMode,
            PersistenceMode,
            PersistenceSettings,
            RunDefaults,
            RunProfile,
        },
        db::workflows::run_profile_validation::matching_groups_for_case,
        models::evaluator::EvaluatorState,
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

    fn dataset_case(task_type: &str, case_group: Option<&str>, tags: &[&str]) -> DatasetCase {
        DatasetCase {
            id: Uuid::now_v7(),
            task_type: task_type.to_string(),
            case_group: case_group.map(str::to_string),
            input: serde_json::Value::Null,
            expected: None,
            context: None,
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            metadata: Default::default(),
        }
    }

    fn evaluator_binding(id: &str, evaluator_ref: &str) -> EvaluatorBinding {
        EvaluatorBinding {
            id: id.to_string(),
            evaluator_ref: evaluator_ref.to_string(),
            required: true,
            dimension: "quality".to_string(),
            blocking: false,
            weight: 1.0,
            normalization: NormalizationPolicy::Normalized,
            pass_threshold: 0.5,
            config: serde_json::json!({}),
        }
    }

    #[test]
    fn matching_groups_use_case_group_override_when_present() {
        let profile = profile();
        let case = dataset_case("different", Some("classification"), &[]);

        let groups = matching_groups_for_case(&profile, &case);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, "classification");
    }

    #[test]
    fn matching_groups_apply_task_type_and_tags() {
        let profile = profile();
        let case = dataset_case("classification", None, &["smoke"]);

        let groups = matching_groups_for_case(&profile, &case);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, "classification");
    }

    #[test]
    fn matching_groups_reject_unknown_overrides_and_unmatched_predicates() {
        let mut profile = profile();
        profile.case_groups[0].applies_to.tags_all = vec!["required".to_string()];

        for case in [
            dataset_case("classification", Some("unknown"), &["smoke", "required"]),
            dataset_case("generation", None, &["smoke", "required"]),
            dataset_case("classification", None, &["required"]),
            dataset_case("classification", None, &["smoke"]),
        ] {
            assert!(matching_groups_for_case(&profile, &case).is_empty());
        }

        let matching = dataset_case("classification", None, &["smoke", "required"]);
        assert_eq!(matching_groups_for_case(&profile, &matching).len(), 1);
    }

    #[test]
    fn valid_profile_has_no_static_config_issues() {
        assert!(super::collect_static_profile_config_issues(&profile()).is_empty());
    }

    #[test]
    fn profile_reports_all_invalid_scalar_configuration() {
        let mut profile = profile();
        profile.agent.provider = " ".to_string();
        profile.agent.name = "".to_string();
        profile.agent.http.url = "not a url".to_string();
        profile.agent.http.method = "not a method".to_string();
        profile.agent.http.timeout_secs = Some(0);
        profile.defaults.max_attempts = 0;
        profile.defaults.min_execution_score = f64::NAN;

        let issues = super::collect_static_profile_config_issues(&profile).join("\n");

        for expected in [
            "agent.provider",
            "agent.name",
            "agent.http.url",
            "agent.http.method",
            "agent.http.timeout_secs",
            "defaults.max_attempts",
            "defaults.min_execution_score",
        ] {
            assert!(issues.contains(expected), "missing issue for {expected}");
        }
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
            dimension: "format".to_string(),
            blocking: true,
            weight: -1.0,
            ..evaluator_binding("schema", "core/json-schema:1.0.0")
        });

        let issues = super::collect_static_profile_config_issues(&profile);

        assert!(issues.iter().any(|issue| issue.contains("weight")));
    }

    #[test]
    fn profile_rejects_invalid_measurement_policy() {
        let mut profile = profile();
        profile.case_groups[0]
            .evaluators
            .push(evaluator_binding("quality", "core/quality:1.0.0"));
        let binding = &mut profile.case_groups[0].evaluators[0];
        binding.pass_threshold = 1.1;
        binding.normalization = NormalizationPolicy::Preference {
            preferred: 1.0,
            tie: f64::NAN,
            not_preferred: 0.0,
        };

        let issues = super::collect_static_profile_config_issues(&profile);

        assert!(issues.iter().any(|issue| issue.contains("pass_threshold")));
        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("preference normalization"))
        );
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
    fn profile_rejects_duplicate_binding_id_within_group() {
        let mut profile = profile();
        profile.case_groups[0].evaluators.push(EvaluatorBinding {
            dimension: "format".to_string(),
            blocking: true,
            ..evaluator_binding("schema", "core/json-schema:1.0.0")
        });
        profile.case_groups[0].evaluators.push(EvaluatorBinding {
            ..evaluator_binding("schema", "core/json-schema:1.0.0")
        });

        let issues = super::collect_static_profile_config_issues(&profile);

        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("duplicate evaluator binding id"))
        );
    }

    #[test]
    fn profile_rejects_non_finite_weights() {
        let mut profile = profile();
        profile.case_groups[0].evaluators.push(EvaluatorBinding {
            dimension: "format".to_string(),
            blocking: true,
            weight: f64::NAN,
            ..evaluator_binding("schema", "core/json-schema:1.0.0")
        });
        profile.case_groups[0].aggregation.dimensions.insert(
            "quality".to_string(),
            DimensionAggregation {
                method: AggregationMethod::WeightedMean,
                blocking: false,
                weight: f64::INFINITY,
            },
        );

        let issues = super::collect_static_profile_config_issues(&profile);

        assert_eq!(
            issues
                .iter()
                .filter(|issue| issue.contains("weight"))
                .count(),
            2
        );
    }

    #[test]
    fn profile_rejects_optional_evaluator_that_affects_policy() {
        let mut profile = profile();
        profile.case_groups[0].evaluators.push(EvaluatorBinding {
            required: false,
            dimension: "diagnostic".to_string(),
            weight: 1.0,
            ..evaluator_binding("diagnostic", "core/diagnostic:1.0.0")
        });

        let issues = super::collect_static_profile_config_issues(&profile);

        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("must be non-blocking with weight 0.0"))
        );
    }

    #[test]
    fn profile_accepts_optional_diagnostic_evaluator() {
        let mut profile = profile();
        profile.case_groups[0].evaluators.extend([
            EvaluatorBinding {
                ..evaluator_binding("scorer", "core/scorer:1.0.0")
            },
            EvaluatorBinding {
                required: false,
                dimension: "diagnostic".to_string(),
                weight: 0.0,
                ..evaluator_binding("diagnostic", "core/diagnostic:1.0.0")
            },
        ]);

        let issues = super::collect_static_profile_config_issues(&profile);

        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
    }

    #[test]
    fn evaluator_runnable_states_match_worker_policy() {
        for state in [
            EvaluatorState::Active,
            EvaluatorState::Deprecated,
            EvaluatorState::Yanked,
        ] {
            assert!(super::is_runnable_state(&state));
        }
        for state in [EvaluatorState::Disabled, EvaluatorState::Removed] {
            assert!(!super::is_runnable_state(&state));
        }
    }
}
