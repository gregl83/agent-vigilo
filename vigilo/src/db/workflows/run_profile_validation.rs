use std::collections::HashMap;

use sqlx::PgPool;

use crate::{
    contracts::{
        evaluator_ref::parse_fully_qualified_evaluator,
        run::{
            DatasetCase,
            RunDataset,
            RunProfile,
        },
    },
    db::tables::evaluators,
    models::evaluator::EvaluatorState,
};

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ProfileExecutabilitySummary {
    pub(crate) expected_execution_count: usize,
    pub(crate) expected_evaluator_execution_count: usize,
    pub(crate) unique_evaluator_ref_count: usize,
    pub(crate) runnable_evaluator_ref_count: usize,
}

pub(crate) async fn validate_profile_executability(
    db: &PgPool,
    profile: &RunProfile,
    dataset: &RunDataset,
) -> anyhow::Result<ProfileExecutabilitySummary> {
    let mut runnable_by_ref: HashMap<String, bool> = HashMap::new();
    let mut issues = Vec::new();

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

            let maybe_evaluator = evaluators::select_evaluator(
                db,
                &identity.namespace,
                &identity.name,
                &identity.version,
            )
            .await?;

            let Some(evaluator_record) = maybe_evaluator else {
                issues.push(format!(
                    "case_group '{}' references unpublished evaluator '{}'; publish it before run create/test",
                    group.id, binding.evaluator_ref
                ));
                runnable_by_ref.insert(binding.evaluator_ref.clone(), false);
                continue;
            };

            if !is_runnable_state(&evaluator_record.state) {
                issues.push(format!(
                    "case_group '{}' references evaluator '{}' in non-runnable state '{:?}'",
                    group.id, binding.evaluator_ref, evaluator_record.state
                ));
                runnable_by_ref.insert(binding.evaluator_ref.clone(), false);
                continue;
            }

            runnable_by_ref.insert(binding.evaluator_ref.clone(), true);
        }
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

fn is_runnable_state(state: &EvaluatorState) -> bool {
    matches!(
        state,
        EvaluatorState::Active | EvaluatorState::Deprecated | EvaluatorState::Yanked
    )
}

#[cfg(test)]
mod tests {
    use crate::{
        contracts::run::{
            AggregationSettings,
            AppliesTo,
            CaseGroupProfile,
            DatasetCase,
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
            id: "c1".to_string(),
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
            id: "c2".to_string(),
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
}
