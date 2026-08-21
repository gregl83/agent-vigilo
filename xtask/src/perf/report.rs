//! Rendering for versioned performance report documents.
//!
//! JSON remains the machine contract. Terminal and Markdown output are derived
//! views and preserve unknown additive JSON fields when an existing report is
//! re-rendered.

use std::{
    fs,
    path::PathBuf,
};

use anyhow::{
    Context,
    Result,
    bail,
};
use clap::Args;

use super::{
    EXIT_PASS,
    artifact::{
        atomic_json,
        atomic_text,
    },
    model::{
        REPORT_SCHEMA,
        ReportDocument,
    },
};

/// CLI arguments for re-rendering an existing report document.
#[derive(Debug, Args)]
pub struct ReportArgs {
    /// Completed performance run directory.
    #[arg(long)]
    run_dir: PathBuf,
}

/// Re-renders terminal and Markdown views from a completed JSON report.
pub fn execute(args: ReportArgs) -> Result<u8> {
    let path = args.run_dir.join("report.json");
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let report: ReportDocument = serde_json::from_slice(&bytes)?;
    if report.schema_id != REPORT_SCHEMA {
        bail!("unsupported report schema: {}", report.schema_id);
    }
    // Rendering never rewrites the machine contract, so unknown additive fields survive.
    atomic_text(&args.run_dir.join("summary.md"), &markdown(&report))?;
    print_terminal(&report, &args.run_dir);
    Ok(EXIT_PASS)
}

/// Atomically writes a report's JSON contract and human-readable views.
pub fn write(run_dir: &std::path::Path, report: &ReportDocument) -> Result<()> {
    atomic_json(&run_dir.join("report.json"), report)?;
    let markdown = markdown(report);
    atomic_text(&run_dir.join("summary.md"), &markdown)?;
    print_terminal(report, run_dir);
    Ok(())
}

/// Prints a concise operator view derived solely from the report contract.
fn print_terminal(report: &ReportDocument, run_dir: &std::path::Path) {
    println!();
    println!(
        "Performance {}: {}",
        report.kind,
        report.status.to_ascii_uppercase()
    );
    println!("Profile: {}", report.profile_id);
    if !report.comparisons.is_empty() {
        println!(
            "{:<34} {:>11} {:>11} {:>10} {:>21} {:>12}",
            "Workload", "Baseline", "Candidate", "Harmful", "95% CI", "Verdict"
        );
        for comparison in &report.comparisons {
            for metric in &comparison.metrics {
                println!(
                    "{:<34} {:>9.3}ms {:>9.3}ms {:>9.2}% [{:>7.2}%, {:>7.2}%] {:>12?}",
                    format!("{}:{}", comparison.workload_id, comparison.tuple_id),
                    metric.baseline_median / 1_000_000.0,
                    metric.candidate_median / 1_000_000.0,
                    metric.harmful_effect * 100.0,
                    metric.confidence_lower * 100.0,
                    metric.confidence_upper * 100.0,
                    metric.verdict
                );
            }
        }
    }
    for failure in &report.failures {
        println!("Failure: {failure}");
    }
    println!("Artifacts: {}", run_dir.display());
}

/// Renders the stable Markdown summary derived from a report document.
fn markdown(report: &ReportDocument) -> String {
    let mut output = format!(
        "# Performance {}\n\n- Status: **{}**\n- Profile: `{}`\n- Run: `{}`\n\n",
        report.kind, report.status, report.profile_id, report.run_id
    );
    if !report.comparisons.is_empty() {
        output.push_str(
            "| Workload | Metric | Baseline | Candidate | Harmful effect | 95% CI | Verdict |\n",
        );
        output.push_str("| --- | --- | ---: | ---: | ---: | ---: | --- |\n");
        for comparison in &report.comparisons {
            for metric in &comparison.metrics {
                output.push_str(&format!(
                    "| `{}`:`{}` | `{}` | {:.3} ms | {:.3} ms | {:+.2}% | [{:+.2}%, {:+.2}%] | `{:?}` |\n",
                    comparison.workload_id,
                    comparison.tuple_id,
                    metric.name,
                    metric.baseline_median / 1_000_000.0,
                    metric.candidate_median / 1_000_000.0,
                    metric.harmful_effect * 100.0,
                    metric.confidence_lower * 100.0,
                    metric.confidence_upper * 100.0,
                    metric.verdict
                ));
            }
        }
        output.push('\n');
    }
    if !report.failures.is_empty() {
        output.push_str("## Failures\n\n");
        for failure in &report.failures {
            output.push_str(&format!("- {failure}\n"));
        }
        output.push('\n');
    }
    output.push_str("## Artifacts\n\n");
    for file in &report.artifact_files {
        output.push_str(&format!("- [`{file}`]({file})\n"));
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::perf::{
        artifact::no_extra,
        model::{
            COMPARISON_SCHEMA,
            ComparisonDocument,
            MetricComparison,
            Verdict,
        },
    };

    fn report() -> ReportDocument {
        ReportDocument {
            schema_id: REPORT_SCHEMA.into(),
            run_id: "run-1".into(),
            kind: "compare".into(),
            status: "failure".into(),
            profile_id: "pr-v1".into(),
            generated_at: "2026-08-17T00:00:00Z".into(),
            comparisons: vec![ComparisonDocument {
                schema_id: COMPARISON_SCHEMA.into(),
                run_id: "run-1".into(),
                profile_id: "pr-v1".into(),
                workload_id: "startup.cli-help.v1".into(),
                tuple_id: "cold-help".into(),
                baseline_digest: "a".into(),
                candidate_digest: "b".into(),
                metrics: vec![MetricComparison {
                    name: "wall_time".into(),
                    unit: "ns".into(),
                    direction: "higher_is_harmful".into(),
                    baseline_median: 1_000_000.0,
                    candidate_median: 1_100_000.0,
                    raw_candidate_delta: 0.1,
                    harmful_effect: 0.1,
                    confidence_lower: 0.05,
                    confidence_upper: 0.15,
                    practical_budget: Some(0.05),
                    verdict: Verdict::Regression,
                    valid_abba_blocks: 1,
                    valid_baab_blocks: 1,
                    unmatched_blocks: 0,
                    residual_orientation_effect: 0.0,
                    orientation_medians: BTreeMap::new(),
                    position_medians: BTreeMap::new(),
                    estimator: "test".into(),
                    bootstrap_seed: 1,
                }],
                verdict: Verdict::Regression,
                extra: no_extra(),
            }],
            failures: vec!["regression confirmed".into()],
            artifact_files: vec!["samples.jsonl".into()],
            extra: no_extra(),
        }
    }

    #[test]
    fn report_round_trip_writes_and_rerenders_human_view() {
        let directory = tempfile::tempdir().unwrap();
        let report = report();
        write(directory.path(), &report).unwrap();
        let first = fs::read_to_string(directory.path().join("summary.md")).unwrap();
        assert!(first.contains("+10.00%"));
        assert!(first.contains("regression confirmed"));
        assert!(first.contains("samples.jsonl"));

        fs::write(directory.path().join("summary.md"), "stale").unwrap();
        assert_eq!(
            execute(ReportArgs {
                run_dir: directory.path().to_path_buf(),
            })
            .unwrap(),
            EXIT_PASS
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("summary.md")).unwrap(),
            first
        );
    }

    #[test]
    fn report_rerender_rejects_unknown_schema() {
        let directory = tempfile::tempdir().unwrap();
        let mut report = report();
        report.schema_id = "report/v2".into();
        fs::write(
            directory.path().join("report.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        assert!(
            execute(ReportArgs {
                run_dir: directory.path().to_path_buf(),
            })
            .is_err()
        );
    }
}
