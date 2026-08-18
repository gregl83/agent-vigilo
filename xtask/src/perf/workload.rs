//! Service-backed MVP workload preparation, execution, and exact oracles.
//!
//! Every measured sample clones a binary-specific prepared PostgreSQL database
//! and receives a fresh RabbitMQ vhost and namespace. Preparation is outside
//! the measurement region; collectors are reset only after all prerequisite
//! work has settled.

use std::{
    collections::BTreeMap,
    path::{
        Path,
        PathBuf,
    },
    time::{
        Duration,
        Instant,
    },
};

use anyhow::{
    Context,
    Result,
    bail,
};
use postgres::{
    Client,
    NoTls,
};
use serde_json::Value;

use super::{
    fixture::{
        FixtureCatalog,
        RunInputs,
        load,
        write_run_inputs,
    },
    model::{
        BuildManifest,
        ExternalMeasurements,
    },
    process::{
        CapturedOutput,
        ProcessOutcome,
        ProcessSpec,
        execute,
    },
    service::{
        DurableCounts,
        SampleScope,
        ServiceHarness,
    },
};

#[derive(Clone)]
struct PreparedDatabase {
    url: String,
    run_id: Option<String>,
    cases: usize,
    evaluators: usize,
}

/// Result of one service-backed workload invocation.
pub struct WorkloadOutcome {
    /// Aggregate process observations for the measured region.
    pub process: ProcessOutcome,
    /// Scoped external measurements and exact durable counts.
    pub external: ExternalMeasurements,
}

pub(super) struct WorkloadRequest<'a> {
    pub(super) workload_id: &'a str,
    pub(super) tuple: &'a str,
    pub(super) fixture_id: &'a str,
    pub(super) binary: &'a Path,
    pub(super) manifest_path: &'a Path,
    pub(super) manifest: &'a BuildManifest,
    pub(super) limits: ExecutionLimits,
}

#[derive(Clone, Copy)]
pub(super) struct ExecutionLimits {
    pub(super) watchdog: Duration,
    pub(super) stdout: usize,
    pub(super) stderr: usize,
}

/// Campaign-owned service topology and reusable prepared database snapshots.
pub struct WorkloadRunner {
    root: PathBuf,
    run_dir: PathBuf,
    run_id: String,
    service: Option<ServiceHarness>,
    fixture: Option<FixtureCatalog>,
    prepared: BTreeMap<String, PreparedDatabase>,
}

impl WorkloadRunner {
    /// Creates a lazy runner; startup-only campaigns never provision services.
    pub fn new(root: &Path, run_dir: &Path, run_id: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            run_dir: run_dir.to_path_buf(),
            run_id: run_id.to_owned(),
            service: None,
            fixture: None,
            prepared: BTreeMap::new(),
        }
    }

    /// Executes one non-startup workload from a fresh isolated sample scope.
    pub fn execute(&mut self, request: WorkloadRequest<'_>) -> Result<WorkloadOutcome> {
        self.ensure_service(request.fixture_id)?;
        let key = format!(
            "{}:{}:{}",
            request.manifest.executable_digest, request.workload_id, request.tuple
        );
        if !self.prepared.contains_key(&key) {
            let prepared = self.prepare(
                request.workload_id,
                request.tuple,
                request.binary,
                request.manifest_path,
                request.limits,
            )?;
            self.prepared.insert(key.clone(), prepared);
        }
        let prepared = self
            .prepared
            .get(&key)
            .context("prepared workload disappeared")?
            .clone();
        let service = self
            .service
            .as_mut()
            .context("service topology was not started")?;
        let template = service.owned_database_name(&prepared.url)?;
        let database_url = service.clone_database(&template, request.workload_id)?;
        let scope = service.create_sample_scope(database_url, request.workload_id)?;

        let result = self.execute_scope(
            request.workload_id,
            request.tuple,
            request.binary,
            request.manifest_path,
            prepared.run_id.as_deref(),
            prepared.cases,
            prepared.evaluators,
            &scope,
            request.limits.watchdog,
            request.limits.stdout,
            request.limits.stderr,
        );
        let release = self
            .service
            .as_mut()
            .context("service topology was not started")?
            .release_scope(scope);
        match (result, release) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error.context("release performance sample scope")),
            (Err(error), Err(cleanup)) => {
                Err(error.context(format!("sample cleanup also failed: {cleanup:#}")))
            }
        }
    }

    fn ensure_service(&mut self, fixture_id: &str) -> Result<()> {
        if let Some(fixture) = &self.fixture {
            if fixture.id != fixture_id {
                bail!("one campaign cannot mix performance fixture catalogs");
            }
            return Ok(());
        }
        let fixture = load(&self.root, fixture_id)?;
        let service = ServiceHarness::start(
            &self.root,
            &self.run_dir,
            &self.run_id,
            &fixture.agent_response_text,
            fixture.agent_payload_bytes,
        )?;
        self.fixture = Some(fixture);
        self.service = Some(service);
        Ok(())
    }

    fn prepare(
        &mut self,
        workload_id: &str,
        tuple: &str,
        binary: &Path,
        manifest_path: &Path,
        limits: ExecutionLimits,
    ) -> Result<PreparedDatabase> {
        let service = self
            .service
            .as_mut()
            .context("service topology was not started")?;
        let database_url = service.create_database(&format!("template-{}", self.prepared.len()))?;
        let migrations = setup_asset(manifest_path, "setup-assets/migrations")?;
        let evaluator = setup_asset(manifest_path, "setup-assets/evaluators/sentiment-basic-en")?;
        require_success(
            invoke(
                binary,
                base_args(
                    &database_url,
                    [
                        "setup".into(),
                        "--migrations-dir".into(),
                        path_arg(&migrations)?,
                        "--skip-evaluators".into(),
                    ],
                ),
                &[],
                Duration::from_secs(120),
                limits.stdout,
                limits.stderr,
            )?,
            "prepare database schema",
        )?;
        require_success(
            invoke(
                binary,
                base_args(
                    &database_url,
                    [
                        "evaluator".into(),
                        "publish".into(),
                        path_arg(&evaluator)?,
                        "--release".into(),
                    ],
                ),
                &[],
                Duration::from_secs(120),
                limits.stdout,
                limits.stderr,
            )?,
            "publish frozen evaluator fixture",
        )?;

        let fixture = self.fixture.as_ref().context("fixture was not loaded")?;
        let (cases, evaluators, create) = match workload_id {
            "run.create.v1" => (fixture.run_create.cases, 1, false),
            "coordinator.dispatch.v1" => (
                fixture.coordinator.chunks * fixture.coordinator.cases_per_chunk,
                1,
                true,
            ),
            "worker.execute-wasm.v1" => match tuple {
                "cases-8-evaluators-1" => (fixture.worker.cases_many, 1, true),
                "cases-1-evaluators-8" => (1, fixture.worker.evaluators_many, true),
                _ => bail!("unsupported worker tuple: {tuple}"),
            },
            "system.lifecycle.v1" => (fixture.lifecycle.cases, 1, false),
            _ => bail!("unsupported service workload: {workload_id}"),
        };
        let run_id = if create {
            let inputs = self.inputs(&format!("{workload_id}:{tuple}"), cases, evaluators)?;
            Some(create_run(
                binary,
                &database_url,
                &inputs,
                &[],
                Duration::from_secs(180),
                limits.stdout,
                limits.stderr,
            )?)
        } else {
            None
        };
        let actual = structural_counts(&database_url, run_id.as_deref())?;
        verify_prepared(workload_id, cases, &actual)?;
        Ok(PreparedDatabase {
            url: database_url,
            run_id,
            cases,
            evaluators,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_scope(
        &mut self,
        workload_id: &str,
        tuple: &str,
        binary: &Path,
        _manifest_path: &Path,
        prepared_run_id: Option<&str>,
        cases: usize,
        evaluators: usize,
        scope: &SampleScope,
        watchdog: Duration,
        stdout_limit: usize,
        stderr_limit: usize,
    ) -> Result<WorkloadOutcome> {
        let env = vec![("VIGILO_MQ_NAMESPACE".into(), scope.mq_namespace.clone())];
        let service = self
            .service
            .as_ref()
            .context("service topology was not started")?;
        let fixture = self.fixture.as_ref().context("fixture was not loaded")?;
        match workload_id {
            "run.create.v1" => {
                let inputs = self.inputs(&format!("{workload_id}:{tuple}"), cases, evaluators)?;
                let baseline = service.begin_measurement(scope)?;
                let process = invoke(
                    binary,
                    create_args(&scope.database_url, &inputs)?,
                    &env,
                    watchdog,
                    stdout_limit,
                    stderr_limit,
                )?;
                let run_id = parse_run_id(&process.stdout.text())?;
                let counts = oracle_create(
                    &scope.database_url,
                    &run_id,
                    cases,
                    fixture.run_create.expected_chunks,
                )?;
                finish(service, scope, baseline, process, counts, 0, 0)
            }
            "coordinator.dispatch.v1" => {
                let run_id = prepared_run_id
                    .context("coordinator fixture has no run")?
                    .to_owned();
                let baseline = service.begin_measurement(scope)?;
                let process = invoke(
                    binary,
                    coordinator_args(&scope.database_url, &scope.messaging_url),
                    &env,
                    watchdog,
                    stdout_limit,
                    stderr_limit,
                )?;
                let counts =
                    oracle_coordinator(&scope.database_url, &run_id, fixture.coordinator.chunks)?;
                finish(
                    service,
                    scope,
                    baseline,
                    process,
                    counts,
                    fixture.coordinator.chunks as u64,
                    0,
                )
            }
            "worker.execute-wasm.v1" => {
                let run_id = prepared_run_id
                    .context("worker fixture has no run")?
                    .to_owned();
                require_success(
                    invoke(
                        binary,
                        coordinator_args(&scope.database_url, &scope.messaging_url),
                        &env,
                        Duration::from_secs(120),
                        stdout_limit,
                        stderr_limit,
                    )?,
                    "prepare worker queue",
                )?;
                let baseline = service.begin_measurement(scope)?;
                let process = invoke(
                    binary,
                    worker_args(&scope.database_url, &scope.messaging_url),
                    &env,
                    watchdog,
                    stdout_limit,
                    stderr_limit,
                )?;
                let counts = oracle_worker(&scope.database_url, &run_id, cases, evaluators)?;
                finish(service, scope, baseline, process, counts, 0, cases as u64)
            }
            "system.lifecycle.v1" => {
                let inputs = self.inputs(&format!("{workload_id}:{tuple}"), cases, evaluators)?;
                let baseline = service.begin_measurement(scope)?;
                let started = Instant::now();
                let create = invoke(
                    binary,
                    create_args(&scope.database_url, &inputs)?,
                    &env,
                    watchdog,
                    stdout_limit,
                    stderr_limit,
                )?;
                require_success_ref(&create, "lifecycle run creation")?;
                let run_id = parse_run_id(&create.stdout.text())?;
                let mut outcomes = vec![create];
                let workers = match tuple {
                    "workers-1" => 1,
                    "workers-2" => 2,
                    _ => bail!("unsupported lifecycle tuple: {tuple}"),
                };
                for _ in 0..fixture.lifecycle.coordinator_cycle_limit {
                    outcomes.push(invoke(
                        binary,
                        coordinator_args(&scope.database_url, &scope.messaging_url),
                        &env,
                        watchdog,
                        stdout_limit,
                        stderr_limit,
                    )?);
                    outcomes.extend(invoke_workers(
                        workers,
                        binary,
                        &scope.database_url,
                        &scope.messaging_url,
                        &env,
                        watchdog,
                        stdout_limit,
                        stderr_limit,
                    )?);
                    outcomes.push(invoke(
                        binary,
                        coordinator_args(&scope.database_url, &scope.messaging_url),
                        &env,
                        watchdog,
                        stdout_limit,
                        stderr_limit,
                    )?);
                    if run_status(&scope.database_url, &run_id)? == "completed" {
                        let counts =
                            oracle_lifecycle(&scope.database_url, &run_id, cases, evaluators)?;
                        let process = aggregate(outcomes, started.elapsed());
                        return finish(service, scope, baseline, process, counts, 0, cases as u64);
                    }
                }
                bail!("lifecycle run {run_id} did not complete within its cycle limit");
            }
            _ => bail!("unsupported service workload: {workload_id}"),
        }
    }

    fn inputs(&self, identity: &str, cases: usize, evaluators: usize) -> Result<RunInputs> {
        let fixture = self.fixture.as_ref().context("fixture was not loaded")?;
        let service = self
            .service
            .as_ref()
            .context("service topology was not started")?;
        let directory = self
            .run_dir
            .join("fixtures")
            .join(identity.replace([':', '\\', '/'], "_"));
        write_run_inputs(
            &directory,
            fixture,
            identity,
            service.agent_url(),
            cases,
            evaluators,
        )
    }
}

impl Drop for WorkloadRunner {
    fn drop(&mut self) {
        if let Some(service) = self.service.as_mut() {
            for prepared in self.prepared.values() {
                if let Err(error) = service.release_database(&prepared.url) {
                    eprintln!("failed to release prepared performance database: {error:#}");
                }
            }
        }
    }
}

fn setup_asset(manifest_path: &Path, relative: &str) -> Result<PathBuf> {
    let path = manifest_path
        .parent()
        .context("build manifest has no parent")?
        .join(relative);
    if !path.exists() {
        bail!("build setup asset is missing: {}", path.display());
    }
    Ok(path)
}

fn base_args<const N: usize>(database_url: &str, command: [String; N]) -> Vec<String> {
    let mut args = vec![
        "--database-url".into(),
        database_url.into(),
        "-q".into(),
        "-f".into(),
        "json".into(),
    ];
    args.extend(command);
    args
}

fn create_args(database_url: &str, inputs: &RunInputs) -> Result<Vec<String>> {
    Ok(base_args(
        database_url,
        [
            "run".into(),
            "create".into(),
            "--profile-file".into(),
            path_arg(&inputs.profile)?,
            "--dataset-file".into(),
            path_arg(&inputs.dataset)?,
        ],
    ))
}

fn coordinator_args(database_url: &str, messaging_url: &str) -> Vec<String> {
    base_args(
        database_url,
        [
            "coordinator".into(),
            "--messaging-url".into(),
            messaging_url.into(),
            "--run-chunk-dispatch-window-size".into(),
            "512".into(),
            "--max-dispatch-per-cycle".into(),
            "128".into(),
            "once".into(),
        ],
    )
}

fn worker_args(database_url: &str, messaging_url: &str) -> Vec<String> {
    base_args(
        database_url,
        [
            "worker".into(),
            "--messaging-url".into(),
            messaging_url.into(),
            "once".into(),
        ],
    )
}

fn invoke(
    binary: &Path,
    args: Vec<String>,
    env: &[(String, String)],
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<ProcessOutcome> {
    execute(&ProcessSpec {
        program: binary,
        args: &args,
        current_dir: None,
        env,
        timeout,
        stdout_limit,
        stderr_limit,
    })
}

#[allow(clippy::too_many_arguments)]
fn invoke_workers(
    count: usize,
    binary: &Path,
    database_url: &str,
    messaging_url: &str,
    env: &[(String, String)],
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<Vec<ProcessOutcome>> {
    std::thread::scope(|scope| {
        let handles = (0..count)
            .map(|_| {
                let args = worker_args(database_url, messaging_url);
                scope.spawn(move || invoke(binary, args, env, timeout, stdout_limit, stderr_limit))
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("worker process thread panicked"))?
            })
            .collect()
    })
}

fn create_run(
    binary: &Path,
    database_url: &str,
    inputs: &RunInputs,
    env: &[(String, String)],
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<String> {
    let outcome = invoke(
        binary,
        create_args(database_url, inputs)?,
        env,
        timeout,
        stdout_limit,
        stderr_limit,
    )?;
    require_success_ref(&outcome, "prepare run fixture")?;
    parse_run_id(&outcome.stdout.text())
}

fn require_success(outcome: ProcessOutcome, context: &str) -> Result<()> {
    require_success_ref(&outcome, context)
}

fn require_success_ref(outcome: &ProcessOutcome, context: &str) -> Result<()> {
    if outcome.timed_out || outcome.exit_code != Some(0) {
        bail!(
            "{context} failed (exit {:?}, timeout {}): {}",
            outcome.exit_code,
            outcome.timed_out,
            outcome.stderr.text()
        );
    }
    Ok(())
}

fn parse_run_id(stdout: &str) -> Result<String> {
    let value: Value = serde_json::from_str(stdout).context("parse run create JSON output")?;
    value["data"]["run_id"]
        .as_str()
        .map(ToOwned::to_owned)
        .context("run create output omitted data.run_id")
}

fn path_arg(path: &Path) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

fn finish(
    service: &ServiceHarness,
    scope: &SampleScope,
    baseline: super::service::MeasurementBaseline,
    process: ProcessOutcome,
    counts: DurableCounts,
    expected_ready: u64,
    expected_http: u64,
) -> Result<WorkloadOutcome> {
    require_success_ref(&process, "measured workload")?;
    let external = service.finish_measurement(scope, baseline, counts)?;
    if external.queue_ready != Some(expected_ready)
        || external.queue_unacked != Some(0)
        || external.http_requests != Some(expected_http)
    {
        bail!(
            "scoped external counts differ: ready={:?}, unacked={:?}, http={:?}; expected {expected_ready}, 0, {expected_http}",
            external.queue_ready,
            external.queue_unacked,
            external.http_requests
        );
    }
    Ok(WorkloadOutcome { process, external })
}

fn scalar(client: &mut Client, query: &str, run_id: &str) -> Result<i64> {
    Ok(client.query_one(query, &[&run_id])?.get(0))
}

fn structural_counts(database_url: &str, run_id: Option<&str>) -> Result<DurableCounts> {
    let mut counts = DurableCounts::new();
    let Some(run_id) = run_id else {
        return Ok(counts);
    };
    let mut client = Client::connect(database_url, NoTls)?;
    for (name, table) in [
        ("runs", "runs"),
        ("chunks", "run_chunks"),
        ("executions", "executions"),
        ("attempts", "execution_attempts"),
        ("evaluator_results", "evaluator_results"),
    ] {
        counts.insert(
            name.into(),
            scalar(
                &mut client,
                &format!("SELECT COUNT(*)::bigint FROM {table} WHERE run_id = $1::uuid"),
                run_id,
            )?,
        );
    }
    Ok(counts)
}

fn verify_prepared(workload_id: &str, cases: usize, counts: &DurableCounts) -> Result<()> {
    if matches!(
        workload_id,
        "coordinator.dispatch.v1" | "worker.execute-wasm.v1"
    ) && (counts.get("runs") != Some(&1)
        || counts.get("executions") != Some(&(cases as i64))
        || counts.get("attempts") != Some(&0)
        || counts.get("evaluator_results") != Some(&0))
    {
        bail!("prepared {workload_id} fixture has unexpected structural counts: {counts:?}");
    }
    Ok(())
}

fn oracle_create(
    database_url: &str,
    run_id: &str,
    cases: usize,
    chunks: i64,
) -> Result<DurableCounts> {
    let mut counts = structural_counts(database_url, Some(run_id))?;
    require_count(&counts, "runs", 1)?;
    require_count(&counts, "chunks", chunks)?;
    require_count(&counts, "executions", cases as i64)?;
    let mut client = Client::connect(database_url, NoTls)?;
    require_status(&mut client, run_id, "pending")?;
    counts.insert("cases".into(), cases as i64);
    Ok(counts)
}

fn oracle_coordinator(database_url: &str, run_id: &str, chunks: usize) -> Result<DurableCounts> {
    let mut counts = structural_counts(database_url, Some(run_id))?;
    let mut client = Client::connect(database_url, NoTls)?;
    require_status(&mut client, run_id, "running")?;
    let dispatched = scalar(
        &mut client,
        "SELECT COUNT(*)::bigint FROM run_chunks WHERE run_id = $1::uuid AND dispatched_at IS NOT NULL",
        run_id,
    )?;
    if dispatched != chunks as i64 {
        bail!("coordinator dispatched {dispatched} chunks; expected {chunks}");
    }
    counts.insert("dispatched_chunks".into(), dispatched);
    let started = scalar(
        &mut client,
        "SELECT COUNT(*)::bigint FROM outbox_events WHERE aggregate_id = $1::uuid AND event_type = 'run.started'",
        run_id,
    )?;
    let ready = scalar(
        &mut client,
        "SELECT COUNT(*)::bigint FROM outbox_events e JOIN run_chunks c ON c.id = e.aggregate_id WHERE c.run_id = $1::uuid AND e.event_type = 'run.chunk.ready'",
        run_id,
    )?;
    if started != 1 || ready != chunks as i64 {
        bail!("coordinator outbox counts differ: run.started={started}, run.chunk.ready={ready}");
    }
    counts.insert("run_started_events".into(), started);
    counts.insert("chunk_ready_events".into(), ready);
    Ok(counts)
}

fn oracle_worker(
    database_url: &str,
    run_id: &str,
    cases: usize,
    evaluators: usize,
) -> Result<DurableCounts> {
    let counts = structural_counts(database_url, Some(run_id))?;
    require_count(&counts, "executions", cases as i64)?;
    require_count(&counts, "attempts", cases as i64)?;
    require_count(
        &counts,
        "evaluator_results",
        cases.saturating_mul(evaluators) as i64,
    )?;
    let mut client = Client::connect(database_url, NoTls)?;
    let completed = scalar(
        &mut client,
        "SELECT COUNT(*)::bigint FROM run_chunks WHERE run_id = $1::uuid AND status = 'completed'",
        run_id,
    )?;
    if completed != 1 {
        bail!("worker completed {completed} chunks; expected 1");
    }
    Ok(counts)
}

fn oracle_lifecycle(
    database_url: &str,
    run_id: &str,
    cases: usize,
    evaluators: usize,
) -> Result<DurableCounts> {
    let counts = oracle_worker(database_url, run_id, cases, evaluators)?;
    let mut client = Client::connect(database_url, NoTls)?;
    require_status(&mut client, run_id, "completed")?;
    let row = client.query_one(
        "SELECT expected_execution_count, terminal_execution_count, passed_execution_count FROM runs WHERE id = $1::uuid",
        &[&run_id],
    )?;
    let expected: i32 = row.get(0);
    let terminal: i32 = row.get(1);
    let passed: i32 = row.get(2);
    if [expected, terminal, passed] != [cases as i32; 3] {
        bail!(
            "lifecycle execution totals differ: expected={expected}, terminal={terminal}, passed={passed}"
        );
    }
    Ok(counts)
}

fn run_status(database_url: &str, run_id: &str) -> Result<String> {
    let mut client = Client::connect(database_url, NoTls)?;
    Ok(client
        .query_one(
            "SELECT status::text FROM runs WHERE id = $1::uuid",
            &[&run_id],
        )?
        .get(0))
}

fn require_status(client: &mut Client, run_id: &str, expected: &str) -> Result<()> {
    let actual: String = client
        .query_one(
            "SELECT status::text FROM runs WHERE id = $1::uuid",
            &[&run_id],
        )?
        .get(0);
    if actual != expected {
        bail!("run {run_id} status is {actual}; expected {expected}");
    }
    Ok(())
}

fn require_count(counts: &DurableCounts, name: &str, expected: i64) -> Result<()> {
    let actual = counts.get(name).copied().unwrap_or_default();
    if actual != expected {
        bail!("durable {name} count is {actual}; expected {expected}");
    }
    Ok(())
}

fn aggregate(outcomes: Vec<ProcessOutcome>, wall_time: Duration) -> ProcessOutcome {
    let mut cpu = Some(0u64);
    let mut rss = Some(0u64);
    let mut stdout = CapturedOutput {
        bytes_seen: 0,
        truncated: false,
        data: Vec::new(),
    };
    let mut stderr = CapturedOutput {
        bytes_seen: 0,
        truncated: false,
        data: Vec::new(),
    };
    let mut exit_code = Some(0);
    let mut timed_out = false;
    for outcome in outcomes {
        cpu = cpu
            .zip(outcome.cpu_time_ns)
            .map(|(left, right)| left.saturating_add(right));
        rss = rss
            .zip(outcome.peak_rss_bytes)
            .map(|(left, right)| left.max(right));
        stdout.bytes_seen = stdout.bytes_seen.saturating_add(outcome.stdout.bytes_seen);
        stdout.truncated |= outcome.stdout.truncated;
        stdout.data.extend(outcome.stdout.data);
        stderr.bytes_seen = stderr.bytes_seen.saturating_add(outcome.stderr.bytes_seen);
        stderr.truncated |= outcome.stderr.truncated;
        stderr.data.extend(outcome.stderr.data);
        timed_out |= outcome.timed_out;
        if outcome.exit_code != Some(0) {
            exit_code = outcome.exit_code;
        }
    }
    ProcessOutcome {
        wall_time,
        cpu_time_ns: cpu,
        peak_rss_bytes: rss,
        resource_source: "aggregate-process-tree",
        exit_code,
        timed_out,
        stdout,
        stderr,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn outcome(
        exit_code: Option<i32>,
        cpu: Option<u64>,
        rss: Option<u64>,
        text: &str,
    ) -> ProcessOutcome {
        ProcessOutcome {
            wall_time: Duration::from_millis(1),
            cpu_time_ns: cpu,
            peak_rss_bytes: rss,
            resource_source: "test",
            exit_code,
            timed_out: false,
            stdout: CapturedOutput {
                bytes_seen: text.len() as u64,
                truncated: false,
                data: text.as_bytes().to_vec(),
            },
            stderr: CapturedOutput {
                bytes_seen: 0,
                truncated: false,
                data: Vec::new(),
            },
        }
    }

    #[test]
    fn command_arguments_keep_service_endpoints_explicit() {
        let args = coordinator_args("postgres://127.0.0.1/db", "amqp://127.0.0.1/vhost");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--database-url", "postgres://127.0.0.1/db"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--messaging-url", "amqp://127.0.0.1/vhost"])
        );

        let worker = worker_args("postgres://127.0.0.1/db", "amqp://127.0.0.1/vhost");
        assert_eq!(worker.last().unwrap(), "once");
        assert_eq!(worker[2..5], ["-q", "-f", "json"]);
    }

    #[test]
    fn run_output_and_structural_count_contracts_fail_closed() {
        assert_eq!(parse_run_id(r#"{"data":{"run_id":"123"}}"#).unwrap(), "123");
        assert!(parse_run_id("not json").is_err());
        assert!(parse_run_id(r#"{"data":{}}"#).is_err());

        let mut counts = DurableCounts::new();
        counts.insert("runs".into(), 1);
        counts.insert("executions".into(), 2);
        counts.insert("attempts".into(), 0);
        counts.insert("evaluator_results".into(), 0);
        assert!(verify_prepared("coordinator.dispatch.v1", 2, &counts).is_ok());
        assert!(verify_prepared("coordinator.dispatch.v1", 3, &counts).is_err());
        assert!(verify_prepared("startup.cli-help.v1", 99, &DurableCounts::new()).is_ok());
        assert!(require_count(&counts, "runs", 1).is_ok());
        assert!(require_count(&counts, "missing", 1).is_err());
        assert!(structural_counts("unused", None).unwrap().is_empty());
    }

    #[test]
    fn process_aggregation_preserves_failures_and_resource_totals() {
        let mut second = outcome(Some(7), Some(3), Some(20), "two");
        second.timed_out = true;
        second.stderr.truncated = true;
        let aggregate = aggregate(
            vec![outcome(Some(0), Some(2), Some(10), "one"), second],
            Duration::from_millis(9),
        );
        assert_eq!(aggregate.wall_time, Duration::from_millis(9));
        assert_eq!(aggregate.cpu_time_ns, Some(5));
        assert_eq!(aggregate.peak_rss_bytes, Some(20));
        assert_eq!(aggregate.exit_code, Some(7));
        assert!(aggregate.timed_out);
        assert_eq!(aggregate.stdout.text(), "onetwo");
        assert!(aggregate.stderr.truncated);
        assert!(require_success(outcome(Some(0), None, None, ""), "test").is_ok());
        assert!(require_success(outcome(Some(1), None, None, ""), "test").is_err());
    }

    #[test]
    fn setup_paths_and_empty_worker_batches_are_service_free() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = directory.path().join("build-manifest.json");
        let asset = directory.path().join("assets");
        fs::write(&manifest, "{}").unwrap();
        fs::create_dir(&asset).unwrap();
        assert_eq!(setup_asset(&manifest, "assets").unwrap(), asset);
        assert!(setup_asset(&manifest, "missing").is_err());
        assert_eq!(path_arg(&manifest).unwrap(), manifest.display().to_string());
        let inputs = RunInputs {
            profile: directory.path().join("profile.yml"),
            dataset: directory.path().join("dataset.yml"),
        };
        let args = create_args("postgres://localhost/db", &inputs).unwrap();
        assert!(args.iter().any(|arg| arg == "--profile-file"));
        assert!(
            invoke_workers(
                0,
                Path::new("unused"),
                "unused",
                "unused",
                &[],
                Duration::from_secs(1),
                1,
                1,
            )
            .unwrap()
            .is_empty()
        );
    }
}
