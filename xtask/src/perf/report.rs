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
