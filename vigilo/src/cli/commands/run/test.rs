use super::*;

/// Implements `vigilo run test`.
///
/// This command performs schema + executability validation and echoes parsed
/// contracts for local inspection without creating persistence records.
pub(super) async fn exec(
    context: Context,
    profile: Option<String>,
    profile_file: Option<PathBuf>,
    dataset: Option<String>,
    dataset_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    let db = context.db().await?;
    let out = context.out().await?;
    let parsed = load_run_inputs(profile, profile_file, dataset, dataset_file)?;
    let executability = run_profile_validation::validate_profile_executability(
        db,
        &parsed.profile,
        &parsed.dataset,
    )
    .await?;

    let payload = json!({
        "data": {
            "profile": parsed.profile,
            "dataset": parsed.dataset,
        },
        "meta": {
            "profile_case_groups": parsed.profile.case_groups.len(),
            "dataset_cases": parsed.dataset.cases.len(),
            "executability": executability,
            "sources": {
                "profile": if parsed.profile_payload.is_object() || parsed.profile_payload.is_array() { "structured" } else { "scalar" },
                "dataset": if parsed.dataset_payload.is_object() || parsed.dataset_payload.is_array() { "structured" } else { "scalar" },
            }
        }
    });

    out.write_value(&payload)?;
    Ok(())
}
