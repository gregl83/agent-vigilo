//! Non-gating rendering of database statement diagnostics.
//!
//! Raw samples remain the source of truth. This module aggregates normalized
//! query fingerprints plus PostgreSQL buffer and WAL counters after a campaign;
//! it never changes correctness, comparison, budget, or model verdicts.

use std::{
    collections::BTreeMap,
    fs,
    path::{
        Path,
        PathBuf,
    },
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
        atomic_text,
        require_artifact_path,
        workspace_root,
    },
    model::Sample,
};

/// Arguments for rendering captured query, buffer, and WAL observations.
#[derive(Debug, Args)]
pub struct DiagnoseArgs {
    /// Completed run directory containing `samples.jsonl`.
    #[arg(long)]
    run_dir: PathBuf,
}

#[derive(Default)]
struct Aggregate {
    query: String,
    samples: u64,
    calls: u64,
    plans: u64,
    rows: u64,
    plan_time_ms: f64,
    time_ms: f64,
    shared_hits: u64,
    shared_reads: u64,
    temporary_blocks: u64,
    wal_records: u64,
    wal_bytes: u64,
}

/// Writes a local Markdown diagnostic without producing a gating result.
pub fn execute(args: DiagnoseArgs) -> Result<u8> {
    let root = workspace_root()?;
    let run_dir = require_artifact_path(&root, &args.run_dir)?;
    let samples = read_samples(&run_dir.join("samples.jsonl"))?;
    let report = render(&samples);
    let output = run_dir.join("diagnostics.md");
    atomic_text(&output, &report)?;
    println!("Diagnostics: {}", output.display());
    Ok(EXIT_PASS)
}

fn read_samples(path: &Path) -> Result<Vec<Sample>> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let samples = content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("parse {} line {}", path.display(), index + 1))
        })
        .collect::<Result<Vec<_>>>()?;
    if samples.is_empty() {
        bail!("diagnostics require at least one raw sample");
    }
    Ok(samples)
}

fn render(samples: &[Sample]) -> String {
    let mut aggregates = BTreeMap::<String, Aggregate>::new();
    for sample in samples {
        for diagnostic in &sample.external.query_diagnostics {
            let aggregate = aggregates
                .entry(diagnostic.query_digest.clone())
                .or_default();
            aggregate.query = diagnostic.query.clone();
            aggregate.samples += 1;
            aggregate.calls = aggregate.calls.saturating_add(diagnostic.calls);
            aggregate.plans = aggregate.plans.saturating_add(diagnostic.plans);
            aggregate.rows = aggregate.rows.saturating_add(diagnostic.rows);
            aggregate.plan_time_ms += diagnostic.total_plan_time_ms;
            aggregate.time_ms += diagnostic.total_exec_time_ms;
            aggregate.shared_hits = aggregate
                .shared_hits
                .saturating_add(diagnostic.shared_blocks_hit);
            aggregate.shared_reads = aggregate
                .shared_reads
                .saturating_add(diagnostic.shared_blocks_read);
            aggregate.temporary_blocks = aggregate
                .temporary_blocks
                .saturating_add(diagnostic.temporary_blocks);
            aggregate.wal_records = aggregate.wal_records.saturating_add(diagnostic.wal_records);
            aggregate.wal_bytes = aggregate.wal_bytes.saturating_add(diagnostic.wal_bytes);
        }
    }
    let mut ranked = aggregates.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|(_, left), (_, right)| right.time_ms.total_cmp(&left.time_ms));
    let mut report = String::from(
        "# Performance Diagnostics\n\nThese observations are collected after timed processes finish and never affect a gate.\n\n",
    );
    if ranked.is_empty() {
        report.push_str("No PostgreSQL statement diagnostics were recorded.\n");
        return report;
    }
    report.push_str("| Query | Samples | Calls/plans | Rows | Plan/exec ms | Shared hit/read | Temp blocks | WAL records/bytes |\n");
    report.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for (digest, aggregate) in ranked {
        let query = aggregate.query.replace(['\r', '\n', '|'], " ");
        report.push_str(&format!(
            "| `{}` `{}` | {} | {}/{} | {} | {:.3}/{:.3} | {}/{} | {} | {}/{} |\n",
            &digest[..digest.len().min(12)],
            query,
            aggregate.samples,
            aggregate.calls,
            aggregate.plans,
            aggregate.rows,
            aggregate.plan_time_ms,
            aggregate.time_ms,
            aggregate.shared_hits,
            aggregate.shared_reads,
            aggregate.temporary_blocks,
            aggregate.wal_records,
            aggregate.wal_bytes,
        ));
    }
    report
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::perf::model::{
        BinaryRole,
        ExternalMeasurements,
        Orientation,
        ProcessMeasurement,
        QueryDiagnostic,
        SAMPLE_SCHEMA,
        SampleState,
        Validation,
    };

    fn sample(diagnostics: Vec<QueryDiagnostic>) -> Sample {
        Sample {
            schema_id: SAMPLE_SCHEMA.into(),
            run_id: "run".into(),
            profile_id: "profile".into(),
            workload_id: "workload".into(),
            tuple_id: "tuple".into(),
            block_id: 0,
            orientation_set_id: 0,
            orientation: Orientation::Single,
            pair_id: 0,
            position: 1,
            role: BinaryRole::Single,
            measured: true,
            started_at: "now".into(),
            process: ProcessMeasurement {
                wall_time_ns: 1,
                cpu_time_ns: None,
                peak_rss_bytes: None,
                resource_source: "test".into(),
                exit_code: Some(0),
                timed_out: false,
                stdout_bytes: 0,
                stderr_bytes: 0,
                stdout_truncated: false,
                stderr_truncated: false,
            },
            validation: Validation {
                state: SampleState::Valid,
                code: "ok".into(),
                message: "ok".into(),
            },
            external: ExternalMeasurements {
                query_diagnostics: diagnostics,
                ..Default::default()
            },
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn report_is_non_gating_with_or_without_statement_data() {
        assert!(render(&[sample(Vec::new())]).contains("No PostgreSQL"));
        let diagnostic = QueryDiagnostic {
            query_digest: "abcdef0123456789".into(),
            query: "SELECT 1".into(),
            calls: 2,
            plans: 1,
            rows: 2,
            total_plan_time_ms: 0.5,
            total_exec_time_ms: 3.5,
            shared_blocks_hit: 4,
            shared_blocks_read: 1,
            temporary_blocks: 0,
            wal_records: 0,
            wal_full_page_images: 0,
            wal_bytes: 0,
        };
        let report = render(&[sample(vec![diagnostic.clone()]), sample(vec![diagnostic])]);
        assert!(report.contains("abcdef012345"));
        assert!(report.contains("| 2 | 4/2 | 4 | 1.000/7.000 | 8/2 |"));
    }

    #[test]
    fn planning_fields_default_when_reading_older_samples() {
        let diagnostic: QueryDiagnostic = serde_json::from_value(serde_json::json!({
            "query_digest": "digest",
            "query": "SELECT 1",
            "calls": 1,
            "rows": 1,
            "total_exec_time_ms": 0.25,
            "shared_blocks_hit": 0,
            "shared_blocks_read": 0,
            "temporary_blocks": 0,
            "wal_records": 0,
            "wal_full_page_images": 0,
            "wal_bytes": 0
        }))
        .unwrap();
        assert_eq!(diagnostic.plans, 0);
        assert_eq!(diagnostic.total_plan_time_ms, 0.0);
    }
}
