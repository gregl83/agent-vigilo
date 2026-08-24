//! Long-running stability and controlled-recovery verdicts.
//!
//! Workload drivers record interval progress, process resources, fault timing,
//! durable work, and protocol amplification. This module applies the versioned
//! registry policy without knowing how services were provisioned. Keeping the
//! verdict pure makes leak, stall, lost-work, and reconnect regressions testable
//! without Docker while the live driver remains the authority for observations.

use std::{
    fs,
    path::Path,
};

use anyhow::{
    Context,
    Result,
    bail,
};
use serde::{
    Deserialize,
    Serialize,
};

use super::{
    artifact::atomic_text,
    model::ReliabilityContract,
};

/// Schema identifier for persisted soak and recovery evidence.
pub const RELIABILITY_SCHEMA: &str = "reliability/v1";

/// One cumulative observation from a resident worker/coordinator pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StabilityObservation {
    /// Seconds elapsed since the measured region began.
    pub elapsed_secs: f64,
    /// Useful cases in terminal completed runs.
    pub completed_cases: u64,
    /// Peak resident bytes across the two Vigilo processes at this point.
    pub process_rss_bytes: Option<u64>,
    /// Open descriptors across the two Vigilo processes at this point.
    pub file_descriptors: Option<u64>,
    /// Whether both resident processes were still running.
    pub processes_running: bool,
}

/// Durable and protocol totals shared by soak and recovery verdicts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReliabilityTotals {
    /// Useful cases the fixture required to reach terminal completion.
    pub expected_cases: u64,
    /// Useful cases observed in terminal completed runs.
    pub completed_cases: u64,
    /// Durable execution attempts produced by those cases.
    pub attempts: u64,
    /// Useful chunks owned by the workload.
    pub chunks: u64,
    /// Worker deliveries observed by the fixture or durable protocol state.
    pub deliveries: u64,
    /// Ready deliveries remaining after settlement.
    pub queue_ready: u64,
    /// Unacknowledged deliveries remaining after settlement.
    pub queue_unacked: u64,
}

/// Machine-readable evidence and verdict retained beside campaign samples.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReliabilityArtifact {
    /// Document shape identifier, currently [`RELIABILITY_SCHEMA`].
    pub schema_id: String,
    /// Workload contract that produced the evidence.
    pub workload_id: String,
    /// Exact fault or steady-state tuple.
    pub tuple_id: String,
    /// `soak` or `recovery`.
    pub kind: String,
    /// Interval observations in chronological order.
    pub observations: Vec<StabilityObservation>,
    /// Exact durable and protocol totals.
    pub totals: ReliabilityTotals,
    /// Seconds between fault injection and restored useful work, if faulted.
    pub recovery_seconds: Option<f64>,
    /// Whether the real fault action completed.
    pub fault_injected: bool,
    /// Whether the bounded verdict passed.
    pub passed: bool,
    /// Every violated contract; an empty list is a pass.
    pub failures: Vec<String>,
}

/// Applies steady-state duration, progress, resource, and amplification bounds.
pub fn evaluate_soak(
    contract: &ReliabilityContract,
    observations: &[StabilityObservation],
    totals: &ReliabilityTotals,
) -> Vec<String> {
    let mut failures = common_failures(contract, observations, totals);
    let Some(first) = observations.first() else {
        failures.push("soak produced no interval observations".into());
        return failures;
    };
    let Some(last) = observations.last() else {
        unreachable!("first observation proves a last observation");
    };
    if last.elapsed_secs + f64::EPSILON < contract.duration_secs as f64 {
        failures.push(format!(
            "soak ended at {:.3}s before the {}s minimum",
            last.elapsed_secs, contract.duration_secs
        ));
    }
    for pair in observations.windows(2) {
        if pair[1].elapsed_secs <= pair[0].elapsed_secs
            || pair[1].completed_cases <= pair[0].completed_cases
        {
            failures.push("soak interval did not make monotonic useful progress".into());
            break;
        }
    }
    if let (Some(first_fds), Some(last_fds)) = (first.file_descriptors, last.file_descriptors) {
        let growth = last_fds.saturating_sub(first_fds);
        if growth > contract.max_file_descriptor_growth {
            failures.push(format!(
                "file descriptors grew by {growth}; limit is {}",
                contract.max_file_descriptor_growth
            ));
        }
    }
    if observations.len() >= 3 {
        let first_rate = interval_rate(&observations[0], &observations[1]);
        let last_rate = interval_rate(
            &observations[observations.len() - 2],
            &observations[observations.len() - 1],
        );
        if first_rate > 0.0 && last_rate / first_rate < contract.min_throughput_retention {
            failures.push(format!(
                "end-window throughput retained {:.3}; minimum is {:.3}",
                last_rate / first_rate,
                contract.min_throughput_retention
            ));
        }
    }
    failures
}

/// Applies exact work, reconnect deadline, process, queue, and amplification bounds.
pub fn evaluate_recovery(
    contract: &ReliabilityContract,
    observations: &[StabilityObservation],
    totals: &ReliabilityTotals,
    fault_injected: bool,
    recovery_seconds: Option<f64>,
) -> Vec<String> {
    let mut failures = common_failures(contract, observations, totals);
    if !fault_injected {
        failures.push("controlled fault was not injected".into());
    }
    match recovery_seconds {
        Some(seconds) if seconds <= contract.recovery_deadline_secs as f64 => {}
        Some(seconds) => failures.push(format!(
            "recovery took {seconds:.3}s; deadline is {}s",
            contract.recovery_deadline_secs
        )),
        None => failures.push("useful work did not recover after the fault".into()),
    }
    failures
}

/// Converts a failure list into a fail-closed workload result.
pub fn require_pass(failures: &[String]) -> Result<()> {
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("reliability contract failed: {}", failures.join("; "))
    }
}

/// Rebuilds a concise Markdown view from retained machine-readable evidence.
pub fn rerender(run_dir: &Path) -> Result<()> {
    let directory = run_dir.join("reliability");
    if !directory.is_dir() {
        return Ok(());
    }
    let mut paths = fs::read_dir(&directory)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort();
    let mut artifacts = Vec::with_capacity(paths.len());
    for path in paths {
        let artifact: ReliabilityArtifact = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
        )?;
        if artifact.schema_id != RELIABILITY_SCHEMA {
            bail!("unsupported reliability schema: {}", artifact.schema_id);
        }
        artifacts.push(artifact);
    }
    let mut output = String::from(
        "# Reliability Evidence\n\n| Workload | Kind | Result | Cases | Attempts | Deliveries | Recovery |\n| --- | --- | --- | ---: | ---: | ---: | ---: |\n",
    );
    for artifact in &artifacts {
        output.push_str(&format!(
            "| `{}`:`{}` | `{}` | **{}** | {}/{} | {} | {} | {} |\n",
            artifact.workload_id,
            artifact.tuple_id,
            artifact.kind,
            if artifact.passed { "pass" } else { "failure" },
            artifact.totals.completed_cases,
            artifact.totals.expected_cases,
            artifact.totals.attempts,
            artifact.totals.deliveries,
            artifact
                .recovery_seconds
                .map_or_else(|| "n/a".into(), |seconds| format!("{seconds:.3}s")),
        ));
        for failure in &artifact.failures {
            output.push_str(&format!("\n- `{}`: {failure}\n", artifact.workload_id));
        }
    }
    atomic_text(&run_dir.join("reliability.md"), &output)
}

fn common_failures(
    contract: &ReliabilityContract,
    observations: &[StabilityObservation],
    totals: &ReliabilityTotals,
) -> Vec<String> {
    let mut failures = Vec::new();
    if totals.completed_cases != totals.expected_cases {
        failures.push(format!(
            "completed {} of {} expected useful cases",
            totals.completed_cases, totals.expected_cases
        ));
    }
    if totals.queue_ready != 0 || totals.queue_unacked != 0 {
        failures.push(format!(
            "queue did not drain: ready={}, unacked={}",
            totals.queue_ready, totals.queue_unacked
        ));
    }
    if totals.completed_cases > 0
        && totals.attempts as f64 / totals.completed_cases as f64 > contract.max_attempts_per_case
    {
        failures.push(format!(
            "attempt amplification {:.3} exceeds {:.3}",
            totals.attempts as f64 / totals.completed_cases as f64,
            contract.max_attempts_per_case
        ));
    }
    if totals.chunks > 0
        && totals.deliveries as f64 / totals.chunks as f64 > contract.max_deliveries_per_chunk
    {
        failures.push(format!(
            "delivery amplification {:.3} exceeds {:.3}",
            totals.deliveries as f64 / totals.chunks as f64,
            contract.max_deliveries_per_chunk
        ));
    }
    if observations
        .iter()
        .any(|observation| !observation.processes_running)
    {
        failures.push("resident worker or coordinator exited before harness shutdown".into());
    }
    if let Some(rss) = observations
        .iter()
        .filter_map(|observation| observation.process_rss_bytes)
        .max()
        && rss > contract.max_process_rss_bytes
    {
        failures.push(format!(
            "process RSS reached {rss} bytes; ceiling is {}",
            contract.max_process_rss_bytes
        ));
    }
    failures
}

fn interval_rate(left: &StabilityObservation, right: &StabilityObservation) -> f64 {
    let seconds = right.elapsed_secs - left.elapsed_secs;
    if seconds <= 0.0 {
        return 0.0;
    }
    right.completed_cases.saturating_sub(left.completed_cases) as f64 / seconds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract() -> ReliabilityContract {
        ReliabilityContract {
            duration_secs: 30,
            observation_interval_secs: 10,
            recovery_deadline_secs: 20,
            max_process_rss_bytes: 1_000,
            max_file_descriptor_growth: 2,
            min_throughput_retention: 0.8,
            max_attempts_per_case: 1.5,
            max_deliveries_per_chunk: 2.0,
        }
    }

    fn observations() -> Vec<StabilityObservation> {
        vec![
            StabilityObservation {
                elapsed_secs: 10.0,
                completed_cases: 10,
                process_rss_bytes: Some(500),
                file_descriptors: Some(10),
                processes_running: true,
            },
            StabilityObservation {
                elapsed_secs: 20.0,
                completed_cases: 20,
                process_rss_bytes: Some(550),
                file_descriptors: Some(11),
                processes_running: true,
            },
            StabilityObservation {
                elapsed_secs: 30.0,
                completed_cases: 30,
                process_rss_bytes: Some(600),
                file_descriptors: Some(12),
                processes_running: true,
            },
        ]
    }

    fn totals() -> ReliabilityTotals {
        ReliabilityTotals {
            expected_cases: 30,
            completed_cases: 30,
            attempts: 30,
            chunks: 3,
            deliveries: 3,
            queue_ready: 0,
            queue_unacked: 0,
        }
    }

    #[test]
    fn valid_soak_and_recovery_evidence_passes() {
        assert!(evaluate_soak(&contract(), &observations(), &totals()).is_empty());
        assert!(
            evaluate_recovery(&contract(), &observations(), &totals(), true, Some(12.0)).is_empty()
        );
    }

    #[test]
    fn leak_stall_lost_work_and_amplification_are_rejected() {
        let mut observations = observations();
        observations[1].completed_cases = 10;
        observations[2].process_rss_bytes = Some(1_001);
        observations[2].file_descriptors = Some(13);
        observations[2].processes_running = false;
        let mut totals = totals();
        totals.completed_cases = 20;
        totals.attempts = 40;
        totals.deliveries = 7;
        totals.queue_ready = 1;
        let failures = evaluate_soak(&contract(), &observations, &totals);
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("completed 20 of 30"))
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("queue did not drain"))
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("attempt amplification"))
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("delivery amplification"))
        );
        assert!(failures.iter().any(|failure| failure.contains("exited")));
        assert!(failures.iter().any(|failure| failure.contains("RSS")));
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("descriptors"))
        );
        assert!(failures.iter().any(|failure| failure.contains("monotonic")));
        assert!(require_pass(&failures).is_err());
    }

    #[test]
    fn missing_or_late_recovery_fails_closed() {
        let missing = evaluate_recovery(&contract(), &observations(), &totals(), false, None);
        assert!(
            missing
                .iter()
                .any(|failure| failure.contains("not injected"))
        );
        assert!(
            missing
                .iter()
                .any(|failure| failure.contains("did not recover"))
        );
        let late = evaluate_recovery(&contract(), &observations(), &totals(), true, Some(20.001));
        assert!(late.iter().any(|failure| failure.contains("deadline")));
    }

    #[test]
    fn machine_evidence_rerenders_without_external_assets() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("reliability")).unwrap();
        let artifact = ReliabilityArtifact {
            schema_id: RELIABILITY_SCHEMA.into(),
            workload_id: "system.recovery.v1".into(),
            tuple_id: "rabbitmq-restart".into(),
            kind: "recovery".into(),
            observations: observations(),
            totals: totals(),
            recovery_seconds: Some(12.0),
            fault_injected: true,
            passed: true,
            failures: Vec::new(),
        };
        fs::write(
            directory
                .path()
                .join("reliability")
                .join("observation.json"),
            serde_json::to_vec(&artifact).unwrap(),
        )
        .unwrap();
        rerender(directory.path()).unwrap();
        let markdown = fs::read_to_string(directory.path().join("reliability.md")).unwrap();
        assert!(markdown.contains("system.recovery.v1"));
        assert!(markdown.contains("12.000s"));
    }
}
