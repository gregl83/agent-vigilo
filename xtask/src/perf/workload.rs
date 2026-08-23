//! Service-backed MVP workload preparation, execution, and exact oracles.
//!
//! Every measured sample clones a binary-specific prepared PostgreSQL database
//! and receives a fresh RabbitMQ vhost and namespace. Preparation is outside
//! the measurement region; collectors are reset only after all prerequisite
//! work has settled.
//!
//! A workload oracle is the exact correctness postcondition for a measured
//! operation: durable row counts, lifecycle state, outbox events, and queue or
//! HTTP observations. It is not an evaluator from the evaluator ABI and it is
//! not a timing threshold. A sample is usable for performance analysis only
//! after its oracle passes. Timing policy belongs to `stats`, while evaluator
//! input/output behavior remains owned by the versioned evaluator ABI.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkloadShape {
    cases: usize,
    evaluators: usize,
    create_run: bool,
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
    pub(super) exact: &'a BTreeMap<String, u64>,
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
            request.exact,
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

    /// Lazily starts one topology and prevents a campaign from mixing fixture catalogs.
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

    /// Builds a reusable, binary-specific database template outside measurement.
    ///
    /// Preparation applies migrations, publishes the frozen evaluator fixture,
    /// creates any prerequisite run, and verifies structural counts before the
    /// database can be cloned for samples.
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
        let shape = workload_shape(fixture, workload_id, tuple)?;
        let run_id = if shape.create_run {
            let inputs = self.inputs(
                &format!("{workload_id}:{tuple}"),
                shape.cases,
                shape.evaluators,
            )?;
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
        verify_prepared(workload_id, &actual)?;
        Ok(PreparedDatabase {
            url: database_url,
            run_id,
            cases: shape.cases,
            evaluators: shape.evaluators,
        })
    }

    #[allow(clippy::too_many_arguments)]
    /// Executes the measured region and exact oracle for one isolated sample scope.
    ///
    /// Setup commands required only to make the sample runnable occur before
    /// `begin_measurement`; each match arm defines the workload-specific measured
    /// boundary and postcondition.
    fn execute_scope(
        &mut self,
        workload_id: &str,
        tuple: &str,
        binary: &Path,
        _manifest_path: &Path,
        prepared_run_id: Option<&str>,
        cases: usize,
        evaluators: usize,
        exact: &BTreeMap<String, u64>,
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
        let (agent_payload, agent_delay_ms) = if workload_id.starts_with("agent.http-") {
            let (_, payload, delay) = parse_http_tuple(tuple)?;
            (payload, delay)
        } else {
            (fixture.agent_payload_bytes, 0)
        };
        service.configure_agent(
            &fixture.agent_response_text,
            agent_payload,
            Duration::from_millis(agent_delay_ms),
        )?;
        match workload_id {
            "run.create.v1" | "run.create-scaling.v1" => {
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
                let chunks = cases.div_ceil(fixture.coordinator.cases_per_chunk) as i64;
                let counts = oracle_create(&scope.database_url, &run_id, cases, chunks)?;
                finish(
                    service,
                    scope,
                    baseline,
                    process,
                    counts,
                    ExternalOracle::new(0, 0, exact),
                )
            }
            "coordinator.dispatch.v1" | "coordinator.dispatch-scaling.v1" => {
                let run_id = prepared_run_id
                    .context("coordinator fixture has no run")?
                    .to_owned();
                let baseline = service.begin_measurement(scope)?;
                let started = Instant::now();
                let process = aggregate(
                    vec![
                        invoke(
                            binary,
                            coordinator_args(&scope.database_url, &scope.messaging_url),
                            &env,
                            watchdog,
                            stdout_limit,
                            stderr_limit,
                        )?,
                        invoke(
                            binary,
                            coordinator_args(&scope.database_url, &scope.messaging_url),
                            &env,
                            watchdog,
                            stdout_limit,
                            stderr_limit,
                        )?,
                    ],
                    started.elapsed(),
                );
                let chunks = cases.div_ceil(fixture.coordinator.cases_per_chunk);
                let counts = oracle_coordinator(&scope.database_url, &run_id, chunks)?;
                finish(
                    service,
                    scope,
                    baseline,
                    process,
                    counts,
                    ExternalOracle::new(chunks as u64, 0, exact),
                )
            }
            "coordinator.outbox-scaling.v1" => {
                let run_id = prepared_run_id
                    .context("outbox fixture has no run")?
                    .to_owned();
                let shape = parse_outbox_tuple(tuple)?;
                require_success(
                    invoke(
                        binary,
                        coordinator_args_with_outbox(
                            &scope.database_url,
                            &scope.messaging_url,
                            1,
                            1,
                        ),
                        &env,
                        Duration::from_secs(180),
                        stdout_limit,
                        stderr_limit,
                    )?,
                    "prepare pending outbox events",
                )?;
                require_success(
                    invoke(
                        binary,
                        coordinator_args_with_outbox(
                            &scope.database_url,
                            &scope.messaging_url,
                            1,
                            1,
                        ),
                        &env,
                        Duration::from_secs(180),
                        stdout_limit,
                        stderr_limit,
                    )?,
                    "dispatch pending outbox events",
                )?;
                service.purge_scope_queues(scope)?;
                let baseline = service.begin_measurement(scope)?;
                let process = invoke(
                    binary,
                    coordinator_args_with_outbox(
                        &scope.database_url,
                        &scope.messaging_url,
                        shape.batch,
                        shape.parallel,
                    ),
                    &env,
                    watchdog,
                    stdout_limit,
                    stderr_limit,
                )?;
                let counts = oracle_coordinator(&scope.database_url, &run_id, shape.events)?;
                finish(
                    service,
                    scope,
                    baseline,
                    process,
                    counts,
                    ExternalOracle::new(shape.batch as u64, 0, exact),
                )
            }
            "coordinator.recovery.v1" => {
                let run_id = prepared_run_id
                    .context("recovery fixture has no run")?
                    .to_owned();
                let leases = parse_single_dimension(tuple, "leases")?;
                require_success(
                    invoke(
                        binary,
                        coordinator_args_with_limits(
                            &scope.database_url,
                            &scope.messaging_url,
                            1000,
                            64,
                            leases,
                            64,
                        ),
                        &env,
                        Duration::from_secs(180),
                        stdout_limit,
                        stderr_limit,
                    )?,
                    "prepare recovery dispatch",
                )?;
                require_success(
                    invoke(
                        binary,
                        coordinator_args_with_limits(
                            &scope.database_url,
                            &scope.messaging_url,
                            leases,
                            64,
                            leases,
                            64,
                        ),
                        &env,
                        Duration::from_secs(180),
                        stdout_limit,
                        stderr_limit,
                    )?,
                    "publish recovery fixture work",
                )?;
                service.purge_scope_queues(scope)?;
                expire_chunk_leases(&scope.database_url, &run_id, leases)?;
                let baseline = service.begin_measurement(scope)?;
                let started = Instant::now();
                let process = aggregate(
                    vec![
                        invoke(
                            binary,
                            coordinator_args_with_limits(
                                &scope.database_url,
                                &scope.messaging_url,
                                leases,
                                64,
                                leases,
                                64,
                            ),
                            &env,
                            watchdog,
                            stdout_limit,
                            stderr_limit,
                        )?,
                        invoke(
                            binary,
                            coordinator_args_with_limits(
                                &scope.database_url,
                                &scope.messaging_url,
                                leases,
                                64,
                                leases,
                                64,
                            ),
                            &env,
                            watchdog,
                            stdout_limit,
                            stderr_limit,
                        )?,
                    ],
                    started.elapsed(),
                );
                let counts = oracle_recovery(&scope.database_url, &run_id, leases)?;
                finish(
                    service,
                    scope,
                    baseline,
                    process,
                    counts,
                    ExternalOracle::new(leases as u64, 0, exact),
                )
            }
            "coordinator.finalization.v1" => {
                let first_run = prepared_run_id
                    .context("finalization fixture has no run")?
                    .to_owned();
                let runs = parse_single_dimension(tuple, "runs")?;
                let mut run_ids = vec![first_run];
                for index in 1..runs {
                    let inputs =
                        self.inputs(&format!("{workload_id}:{tuple}:run-{index}"), 1, evaluators)?;
                    run_ids.push(create_run(
                        binary,
                        &scope.database_url,
                        &inputs,
                        &env,
                        Duration::from_secs(180),
                        stdout_limit,
                        stderr_limit,
                    )?);
                }
                require_success(
                    invoke(
                        binary,
                        coordinator_args_with_limits(
                            &scope.database_url,
                            &scope.messaging_url,
                            runs,
                            64,
                            1000,
                            runs,
                        ),
                        &env,
                        Duration::from_secs(180),
                        stdout_limit,
                        stderr_limit,
                    )?,
                    "prepare finalization dispatch",
                )?;
                require_success(
                    invoke(
                        binary,
                        coordinator_args_with_limits(
                            &scope.database_url,
                            &scope.messaging_url,
                            runs,
                            64,
                            1000,
                            runs,
                        ),
                        &env,
                        Duration::from_secs(180),
                        stdout_limit,
                        stderr_limit,
                    )?,
                    "publish finalization fixture work",
                )?;
                service.settled_queue_counts(scope, runs as u64, 0)?;
                for _ in 0..fixture.lifecycle.worker_pass_limit {
                    invoke_workers_batched(
                        runs,
                        8,
                        binary,
                        &scope.database_url,
                        &scope.messaging_url,
                        &env,
                        watchdog,
                        stdout_limit,
                        stderr_limit,
                    )?;
                    if terminal_executions(&scope.database_url)? == runs as i64 {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
                require_terminal_executions(&scope.database_url, runs)?;
                let baseline = service.begin_measurement(scope)?;
                let process = invoke(
                    binary,
                    coordinator_args_with_limits(
                        &scope.database_url,
                        &scope.messaging_url,
                        runs,
                        64,
                        1000,
                        runs,
                    ),
                    &env,
                    watchdog,
                    stdout_limit,
                    stderr_limit,
                )?;
                let counts = oracle_finalization(&scope.database_url, runs)?;
                let _ = run_ids;
                finish(
                    service,
                    scope,
                    baseline,
                    process,
                    counts,
                    ExternalOracle::new(0, 0, exact),
                )
            }
            "worker.execute-wasm.v1"
            | "agent.http-scaling.v1"
            | "agent.http-variants.v1"
            | "evaluator.wasm-scaling.v1"
            | "worker.persistence-scaling.v1" => {
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
                require_success(
                    invoke(
                        binary,
                        coordinator_args(&scope.database_url, &scope.messaging_url),
                        &env,
                        Duration::from_secs(120),
                        stdout_limit,
                        stderr_limit,
                    )?,
                    "publish worker queue",
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
                finish(
                    service,
                    scope,
                    baseline,
                    process,
                    counts,
                    ExternalOracle::new(0, cases as u64, exact),
                )
            }
            "system.lifecycle.v1" | "system.capacity.v1" => {
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
                let workers = lifecycle_workers(tuple)?;
                let cycle_limit = if workload_id == "system.capacity.v1" {
                    fixture.lifecycle.capacity_cycle_limit
                } else {
                    fixture.lifecycle.coordinator_cycle_limit
                };
                for _ in 0..cycle_limit {
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
                        return finish(
                            service,
                            scope,
                            baseline,
                            process,
                            counts,
                            ExternalOracle::new(0, cases as u64, exact),
                        );
                    }
                }
                bail!("lifecycle run {run_id} did not complete within its cycle limit");
            }
            _ => bail!("unsupported service workload: {workload_id}"),
        }
    }

    /// Renders deterministic profile and dataset inputs beneath the campaign directory.
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

/// Resolves a registered workload tuple into its setup cardinalities.
///
/// Unknown workload IDs and tuples fail closed; future workload versions add
/// new match arms without changing callers or shared service infrastructure.
fn workload_shape(
    fixture: &FixtureCatalog,
    workload_id: &str,
    tuple: &str,
) -> Result<WorkloadShape> {
    let shape = match workload_id {
        "run.create.v1" => WorkloadShape {
            cases: fixture.run_create.cases,
            evaluators: 1,
            create_run: false,
        },
        "run.create-scaling.v1" => WorkloadShape {
            cases: parse_single_dimension(tuple, "cases")?,
            evaluators: 1,
            create_run: false,
        },
        "coordinator.dispatch.v1" => WorkloadShape {
            cases: fixture.coordinator.chunks * fixture.coordinator.cases_per_chunk,
            evaluators: 1,
            create_run: true,
        },
        "coordinator.dispatch-scaling.v1" => WorkloadShape {
            cases: parse_single_dimension(tuple, "chunks")?
                .checked_mul(fixture.coordinator.cases_per_chunk)
                .context("coordinator workload cardinality overflow")?,
            evaluators: 1,
            create_run: true,
        },
        "coordinator.outbox-scaling.v1" => WorkloadShape {
            cases: parse_outbox_tuple(tuple)?
                .events
                .checked_mul(fixture.coordinator.cases_per_chunk)
                .context("outbox workload cardinality overflow")?,
            evaluators: 1,
            create_run: true,
        },
        "coordinator.recovery.v1" => WorkloadShape {
            cases: parse_single_dimension(tuple, "leases")?
                .checked_mul(fixture.coordinator.cases_per_chunk)
                .context("recovery workload cardinality overflow")?,
            evaluators: 1,
            create_run: true,
        },
        "coordinator.finalization.v1" => WorkloadShape {
            cases: 1,
            evaluators: 1,
            create_run: true,
        },
        "worker.execute-wasm.v1" => match tuple {
            "cases-8-evaluators-1" => WorkloadShape {
                cases: fixture.worker.cases_many,
                evaluators: 1,
                create_run: true,
            },
            "cases-1-evaluators-8" => WorkloadShape {
                cases: 1,
                evaluators: fixture.worker.evaluators_many,
                create_run: true,
            },
            _ => bail!("unsupported worker tuple: {tuple}"),
        },
        "agent.http-scaling.v1" | "agent.http-variants.v1" => WorkloadShape {
            cases: parse_http_tuple(tuple)?.0,
            evaluators: 1,
            create_run: true,
        },
        "evaluator.wasm-scaling.v1" => WorkloadShape {
            cases: 1,
            evaluators: parse_single_dimension(tuple, "evaluators")?,
            create_run: true,
        },
        "worker.persistence-scaling.v1" => WorkloadShape {
            cases: parse_single_dimension(tuple, "cases")?,
            evaluators: 1,
            create_run: true,
        },
        "system.lifecycle.v1" => WorkloadShape {
            cases: fixture.lifecycle.cases,
            evaluators: 1,
            create_run: false,
        },
        "system.capacity.v1" => {
            let (_, load) = capacity_tuple(tuple)?;
            WorkloadShape {
                cases: fixture.lifecycle.cases.saturating_mul(load),
                evaluators: 1,
                create_run: false,
            }
        }
        _ => bail!("unsupported service workload: {workload_id}"),
    };
    Ok(shape)
}

fn parse_single_dimension(tuple: &str, dimension: &str) -> Result<usize> {
    let (actual, value) = tuple
        .split_once('-')
        .with_context(|| format!("invalid {dimension} tuple: {tuple}"))?;
    if actual != dimension {
        bail!("invalid {dimension} tuple: {tuple}");
    }
    let value = value.parse::<usize>()?;
    if value == 0 {
        bail!("{dimension} tuple must be positive");
    }
    Ok(value)
}

fn parse_http_tuple(tuple: &str) -> Result<(usize, usize, u64)> {
    let parts = tuple.split('-').collect::<Vec<_>>();
    if parts.len() != 6 || parts[0] != "cases" || parts[2] != "payload" || parts[4] != "delay" {
        bail!("invalid HTTP workload tuple: {tuple}");
    }
    let cases = parts[1].parse::<usize>()?;
    let payload = parts[3].parse::<usize>()?;
    let delay = parts[5].parse::<u64>()?;
    if cases == 0 || payload == 0 {
        bail!("HTTP workload cases and payload must be positive");
    }
    Ok((cases, payload, delay))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutboxShape {
    events: usize,
    batch: usize,
    parallel: usize,
}

fn parse_outbox_tuple(tuple: &str) -> Result<OutboxShape> {
    let parts = tuple.split('-').collect::<Vec<_>>();
    if parts.len() != 6 || parts[0] != "events" || parts[2] != "batch" || parts[4] != "parallel" {
        bail!("invalid outbox workload tuple: {tuple}");
    }
    let shape = OutboxShape {
        events: parts[1].parse()?,
        batch: parts[3].parse()?,
        parallel: parts[5].parse()?,
    };
    if shape.events == 0
        || !matches!(shape.batch, 1 | 64 | 65 | 256 | 1000)
        || !matches!(shape.parallel, 1 | 8 | 64)
    {
        bail!("unsupported outbox workload tuple: {tuple}");
    }
    Ok(shape)
}

/// Resolves the lifecycle tuple into the number of measured worker processes.
fn lifecycle_workers(tuple: &str) -> Result<usize> {
    match tuple {
        "workers-1" => Ok(1),
        "workers-2" => Ok(2),
        _ => capacity_tuple(tuple)
            .map(|(workers, _)| workers)
            .map_err(|_| anyhow::anyhow!("unsupported lifecycle tuple: {tuple}")),
    }
}

/// Parses one predeclared worker/load capacity-staircase tuple.
pub(super) fn capacity_tuple(tuple: &str) -> Result<(usize, usize)> {
    let parts = tuple.split('-').collect::<Vec<_>>();
    if parts.len() != 4 || parts[0] != "workers" || parts[2] != "load" {
        bail!("unsupported capacity tuple: {tuple}");
    }
    let workers = parts[1].parse::<usize>()?;
    let load = parts[3].parse::<usize>()?;
    if !matches!(workers, 1 | 2) || !matches!(load, 1 | 2 | 4 | 8 | 16) {
        bail!("unsupported capacity tuple: {tuple}");
    }
    Ok((workers, load))
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

/// Resolves a required setup asset relative to its immutable build manifest.
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

/// Prefixes a Vigilo command with explicit database and machine-output settings.
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
    coordinator_args_with_outbox(database_url, messaging_url, 1000, 64)
}

fn coordinator_args_with_outbox(
    database_url: &str,
    messaging_url: &str,
    outbox_batch: usize,
    publish_parallelism: usize,
) -> Vec<String> {
    coordinator_args_with_limits(
        database_url,
        messaging_url,
        outbox_batch,
        publish_parallelism,
        1000,
        64,
    )
}

fn coordinator_args_with_limits(
    database_url: &str,
    messaging_url: &str,
    outbox_batch: usize,
    publish_parallelism: usize,
    recovery_batch: usize,
    finalize_limit: usize,
) -> Vec<String> {
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
            "--max-finalize-per-cycle".into(),
            finalize_limit.to_string(),
            "--chunk-lease-recovery-batch-size".into(),
            recovery_batch.to_string(),
            "--outbox-batch-size".into(),
            outbox_batch.to_string(),
            "--outbox-publish-parallelism".into(),
            publish_parallelism.to_string(),
            "once".into(),
        ],
    )
}

#[allow(clippy::too_many_arguments)]
fn invoke_workers_batched(
    count: usize,
    batch: usize,
    binary: &Path,
    database_url: &str,
    messaging_url: &str,
    env: &[(String, String)],
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<()> {
    for workers in (0..count).collect::<Vec<_>>().chunks(batch) {
        for outcome in invoke_workers(
            workers.len(),
            binary,
            database_url,
            messaging_url,
            env,
            timeout,
            stdout_limit,
            stderr_limit,
        )? {
            require_success(outcome, "prepare finalization worker")?;
        }
    }
    Ok(())
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

/// Runs one bounded Vigilo process without a shell.
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
/// Runs the requested worker count concurrently and returns every process outcome.
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

/// Creates a prerequisite run and extracts its stable machine-readable ID.
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

/// Exact external postconditions that a measured process must produce.
struct ExternalOracle<'a> {
    ready: u64,
    http: u64,
    exact: &'a BTreeMap<String, u64>,
}

impl<'a> ExternalOracle<'a> {
    fn new(ready: u64, http: u64, exact: &'a BTreeMap<String, u64>) -> Self {
        Self { ready, http, exact }
    }
}

/// Finalizes a measured region and enforces scoped external postconditions.
///
/// Process success, queue drain state, and deterministic-agent request counts
/// must all match before the observations become a workload outcome.
fn finish(
    service: &ServiceHarness,
    scope: &SampleScope,
    baseline: super::service::MeasurementBaseline,
    process: ProcessOutcome,
    counts: DurableCounts,
    oracle: ExternalOracle<'_>,
) -> Result<WorkloadOutcome> {
    require_success_ref(&process, "measured workload")?;
    let mut external = service.finish_measurement(scope, baseline, counts)?;
    let queue = service.settled_queue_counts(scope, oracle.ready, 0)?;
    external.queue_ready = Some(queue.0);
    external.queue_unacked = Some(queue.1);
    if external.queue_ready != Some(oracle.ready)
        || external.queue_unacked != Some(0)
        || external.http_requests != Some(oracle.http)
    {
        bail!(
            "scoped external counts differ: ready={:?}, unacked={:?}, http={:?}; expected {}, 0, {}",
            external.queue_ready,
            external.queue_unacked,
            external.http_requests,
            oracle.ready,
            oracle.http,
        );
    }
    verify_exact_observations(&external, oracle.exact)?;
    Ok(WorkloadOutcome { process, external })
}

/// Applies registry-owned exact amplification and durable-output gates.
fn verify_exact_observations(
    external: &ExternalMeasurements,
    expected: &BTreeMap<String, u64>,
) -> Result<()> {
    for (metric, expected) in expected {
        let actual = match metric.as_str() {
            "http_requests" => external.http_requests,
            "queue_ready" => external.queue_ready,
            "queue_unacked" => external.queue_unacked,
            metric if metric.starts_with("durable.") => external
                .durable_counts
                .get(metric.trim_start_matches("durable."))
                .copied()
                .map(|value| u64::try_from(value).context("durable count was negative"))
                .transpose()?,
            _ => bail!("unsupported exact observation {metric}"),
        };
        if actual != Some(*expected) {
            bail!("exact observation {metric} differed: actual {actual:?}, expected {expected}");
        }
    }
    Ok(())
}

fn scalar(client: &mut Client, query: &str, run_id: &str) -> Result<i64> {
    Ok(client.query_one(query, &[&run_id])?.get(0))
}

/// Reads the common durable row counts used by preparation and workload oracles.
fn structural_counts(database_url: &str, run_id: Option<&str>) -> Result<DurableCounts> {
    let mut counts = DurableCounts::new();
    let Some(run_id) = run_id else {
        return Ok(counts);
    };
    let mut client = Client::connect(database_url, NoTls)?;
    for (name, table, run_column) in [
        ("runs", "runs", "id"),
        ("chunks", "run_chunks", "run_id"),
        ("executions", "executions", "run_id"),
        ("attempts", "execution_attempts", "run_id"),
        ("evaluator_results", "evaluator_results", "run_id"),
    ] {
        counts.insert(
            name.into(),
            scalar(
                &mut client,
                &format!(
                    "SELECT COUNT(*)::bigint FROM {table} WHERE {run_column} = $1::text::uuid"
                ),
                run_id,
            )?,
        );
    }
    Ok(counts)
}

/// Verifies reusable coordinator and worker templates contain no measured results.
fn verify_prepared(workload_id: &str, counts: &DurableCounts) -> Result<()> {
    if matches!(
        workload_id,
        "coordinator.dispatch.v1"
            | "coordinator.dispatch-scaling.v1"
            | "coordinator.outbox-scaling.v1"
            | "coordinator.recovery.v1"
            | "coordinator.finalization.v1"
            | "worker.execute-wasm.v1"
            | "agent.http-scaling.v1"
            | "agent.http-variants.v1"
            | "evaluator.wasm-scaling.v1"
            | "worker.persistence-scaling.v1"
    ) && (counts.get("runs") != Some(&1)
        || counts.get("executions") != Some(&0)
        || counts.get("attempts") != Some(&0)
        || counts.get("evaluator_results") != Some(&0))
    {
        bail!("prepared {workload_id} fixture has unexpected structural counts: {counts:?}");
    }
    Ok(())
}

/// Collects and validates the run-creation durable-state oracle.
fn oracle_create(
    database_url: &str,
    run_id: &str,
    cases: usize,
    chunks: i64,
) -> Result<DurableCounts> {
    let mut counts = structural_counts(database_url, Some(run_id))?;
    let mut client = Client::connect(database_url, NoTls)?;
    let status = run_status_with(&mut client, run_id)?;
    validate_create_oracle(&counts, &status, chunks)?;
    counts.insert("cases".into(), cases as i64);
    Ok(counts)
}

/// Collects and validates dispatch state plus exact start and chunk-ready events.
fn oracle_coordinator(database_url: &str, run_id: &str, chunks: usize) -> Result<DurableCounts> {
    let mut counts = structural_counts(database_url, Some(run_id))?;
    let mut client = Client::connect(database_url, NoTls)?;
    let status = run_status_with(&mut client, run_id)?;
    let dispatched = scalar(
        &mut client,
        "SELECT COUNT(*)::bigint FROM run_chunks WHERE run_id = $1::text::uuid AND dispatched_at IS NOT NULL",
        run_id,
    )?;
    let started = scalar(
        &mut client,
        "SELECT COUNT(*)::bigint FROM outbox_events WHERE aggregate_id = $1::text::uuid AND event_type = 'run.started'",
        run_id,
    )?;
    let ready = scalar(
        &mut client,
        "SELECT COUNT(*)::bigint FROM outbox_events WHERE aggregate_id = $1::text::uuid AND event_type = 'run.chunk.ready'",
        run_id,
    )?;
    validate_coordinator_oracle(&status, dispatched, started, ready, chunks)?;
    counts.insert("dispatched_chunks".into(), dispatched);
    counts.insert("run_started_events".into(), started);
    counts.insert("chunk_ready_events".into(), ready);
    Ok(counts)
}

/// Collects and validates worker execution, attempt, result, and chunk counts.
fn oracle_worker(
    database_url: &str,
    run_id: &str,
    cases: usize,
    evaluators: usize,
) -> Result<DurableCounts> {
    let counts = structural_counts(database_url, Some(run_id))?;
    let mut client = Client::connect(database_url, NoTls)?;
    let completed = scalar(
        &mut client,
        "SELECT COUNT(*)::bigint FROM run_chunks WHERE run_id = $1::text::uuid AND status = 'completed'",
        run_id,
    )?;
    validate_worker_oracle(&counts, completed, cases, evaluators)?;
    Ok(counts)
}

/// Extends the worker oracle with terminal passing-run invariants.
fn oracle_lifecycle(
    database_url: &str,
    run_id: &str,
    cases: usize,
    evaluators: usize,
) -> Result<DurableCounts> {
    let counts = oracle_worker(database_url, run_id, cases, evaluators)?;
    let mut client = Client::connect(database_url, NoTls)?;
    let status = run_status_with(&mut client, run_id)?;
    let row = client.query_one(
        "SELECT expected_execution_count, terminal_execution_count, passed_execution_count FROM runs WHERE id = $1::text::uuid",
        &[&run_id],
    )?;
    let expected: i32 = row.get(0);
    let terminal: i32 = row.get(1);
    let passed: i32 = row.get(2);
    validate_lifecycle_oracle(&status, [expected, terminal, passed], cases)?;
    Ok(counts)
}

fn expire_chunk_leases(database_url: &str, run_id: &str, expected: usize) -> Result<()> {
    let mut client = Client::connect(database_url, NoTls)?;
    let changed = client.execute(
        r#"
        UPDATE run_chunks
        SET status = 'leased',
            lease_token = '00000000-0000-7000-8000-000000000001'::uuid,
            leased_until = now() - interval '1 minute',
            updated_at = now()
        WHERE run_id = $1::text::uuid
        "#,
        &[&run_id],
    )?;
    if changed != expected as u64 {
        bail!("expired {changed} recovery leases; expected {expected}");
    }
    Ok(())
}

fn oracle_recovery(database_url: &str, run_id: &str, expected: usize) -> Result<DurableCounts> {
    let mut counts = structural_counts(database_url, Some(run_id))?;
    let mut client = Client::connect(database_url, NoTls)?;
    let recovered = scalar(
        &mut client,
        "SELECT COUNT(*)::bigint FROM run_chunks WHERE run_id = $1::text::uuid AND recovery_count = 1 AND status = 'pending'",
        run_id,
    )?;
    let events = scalar(
        &mut client,
        "SELECT COUNT(*)::bigint FROM outbox_events WHERE aggregate_id = $1::text::uuid AND event_type = 'run.chunk.ready' AND dedupe_key LIKE '%:recovery:1'",
        run_id,
    )?;
    if recovered != expected as i64 || events != expected as i64 {
        bail!(
            "recovery oracle differs: recovered={recovered}, events={events}, expected={expected}"
        );
    }
    counts.insert("recovered_chunks".into(), recovered);
    counts.insert("recovery_events".into(), events);
    Ok(counts)
}

fn require_terminal_executions(database_url: &str, expected: usize) -> Result<()> {
    let terminal = terminal_executions(database_url)?;
    if terminal != expected as i64 {
        let mut client = Client::connect(database_url, NoTls)?;
        let rows = client.query(
            "SELECT status::text, COUNT(*)::bigint FROM executions GROUP BY status ORDER BY status::text",
            &[],
        )?;
        let statuses = rows
            .into_iter()
            .map(|row| format!("{}={}", row.get::<_, String>(0), row.get::<_, i64>(1)))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "prepared {terminal} terminal executions; expected {expected}; observed [{statuses}]"
        );
    }
    Ok(())
}

fn terminal_executions(database_url: &str) -> Result<i64> {
    let mut client = Client::connect(database_url, NoTls)?;
    let completed: i64 = client
        .query_one(
            "SELECT COUNT(*)::bigint FROM executions WHERE status IN ('completed', 'failed', 'timed_out', 'cancelled')",
            &[],
        )?
        .get(0);
    Ok(completed)
}

fn oracle_finalization(database_url: &str, expected: usize) -> Result<DurableCounts> {
    let mut client = Client::connect(database_url, NoTls)?;
    let values: [i64; 4] = [
        client
            .query_one("SELECT COUNT(*)::bigint FROM runs", &[])?
            .get(0),
        client
            .query_one(
                "SELECT COUNT(*)::bigint FROM runs WHERE status = 'completed'",
                &[],
            )?
            .get(0),
        client
            .query_one("SELECT COUNT(*)::bigint FROM evaluator_results", &[])?
            .get(0),
        client
            .query_one(
                "SELECT COUNT(*)::bigint FROM outbox_events WHERE event_type = 'run.completed'",
                &[],
            )?
            .get(0),
    ];
    if values != [expected as i64; 4] {
        bail!(
            "finalization oracle differs: runs={}, completed={}, results={}, events={}, expected={expected}",
            values[0],
            values[1],
            values[2],
            values[3]
        );
    }
    Ok(DurableCounts::from([
        ("runs".into(), values[0]),
        ("completed_runs".into(), values[1]),
        ("evaluator_results".into(), values[2]),
        ("completion_events".into(), values[3]),
    ]))
}

/// Applies the pure run-creation postcondition to collected observations.
fn validate_create_oracle(counts: &DurableCounts, status: &str, chunks: i64) -> Result<()> {
    require_count(counts, "runs", 1)?;
    require_count(counts, "chunks", chunks)?;
    require_count(counts, "executions", 0)?;
    require_status_value(status, "pending")
}

/// Applies the pure coordinator postcondition to collected observations.
fn validate_coordinator_oracle(
    status: &str,
    dispatched: i64,
    started: i64,
    ready: i64,
    chunks: usize,
) -> Result<()> {
    require_status_value(status, "running")?;
    if dispatched != chunks as i64 {
        bail!("coordinator dispatched {dispatched} chunks; expected {chunks}");
    }
    if started != 1 || ready != chunks as i64 {
        bail!("coordinator outbox counts differ: run.started={started}, run.chunk.ready={ready}");
    }
    Ok(())
}

/// Applies the pure worker/Wasm postcondition to collected observations.
fn validate_worker_oracle(
    counts: &DurableCounts,
    completed: i64,
    cases: usize,
    evaluators: usize,
) -> Result<()> {
    require_count(counts, "executions", cases as i64)?;
    require_count(counts, "attempts", cases as i64)?;
    require_count(
        counts,
        "evaluator_results",
        cases.saturating_mul(evaluators) as i64,
    )?;
    if completed != 1 {
        bail!("worker completed {completed} chunks; expected 1");
    }
    Ok(())
}

/// Applies the pure end-to-end lifecycle postcondition to collected observations.
fn validate_lifecycle_oracle(status: &str, totals: [i32; 3], cases: usize) -> Result<()> {
    require_status_value(status, "completed")?;
    if totals != [cases as i32; 3] {
        bail!(
            "lifecycle execution totals differ: expected={}, terminal={}, passed={}",
            totals[0],
            totals[1],
            totals[2]
        );
    }
    Ok(())
}

fn run_status(database_url: &str, run_id: &str) -> Result<String> {
    let mut client = Client::connect(database_url, NoTls)?;
    run_status_with(&mut client, run_id)
}

fn run_status_with(client: &mut Client, run_id: &str) -> Result<String> {
    Ok(client
        .query_one(
            "SELECT status::text FROM runs WHERE id = $1::text::uuid",
            &[&run_id],
        )?
        .get(0))
}

fn require_status_value(actual: &str, expected: &str) -> Result<()> {
    if actual != expected {
        bail!("run status is {actual}; expected {expected}");
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

/// Combines a multi-process lifecycle region into one conservative observation.
///
/// CPU is summed, peak memory is maximized, output is concatenated, and any
/// timeout or nonzero exit is retained.
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
        let outbox = coordinator_args_with_outbox(
            "postgres://127.0.0.1/db",
            "amqp://127.0.0.1/vhost",
            65,
            8,
        );
        assert!(
            outbox
                .windows(2)
                .any(|pair| pair == ["--outbox-batch-size", "65"])
        );
        assert!(
            outbox
                .windows(2)
                .any(|pair| pair == ["--outbox-publish-parallelism", "8"])
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
        counts.insert("executions".into(), 0);
        counts.insert("attempts".into(), 0);
        counts.insert("evaluator_results".into(), 0);
        assert!(verify_prepared("coordinator.dispatch.v1", &counts).is_ok());
        counts.insert("executions".into(), 1);
        assert!(verify_prepared("coordinator.dispatch.v1", &counts).is_err());
        assert!(verify_prepared("startup.cli-help.v1", &DurableCounts::new()).is_ok());
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

    #[test]
    fn completed_workload_shapes_resolve_and_unknown_shapes_fail_closed() {
        let root = crate::perf::artifact::workspace_root().unwrap();
        let fixture = load(&root, "mvp-v1").unwrap();

        assert_eq!(
            workload_shape(&fixture, "run.create.v1", "cases-1001").unwrap(),
            WorkloadShape {
                cases: 1001,
                evaluators: 1,
                create_run: false,
            }
        );
        assert_eq!(
            workload_shape(&fixture, "coordinator.dispatch.v1", "chunks-512").unwrap(),
            WorkloadShape {
                cases: 51_200,
                evaluators: 1,
                create_run: true,
            }
        );
        assert_eq!(
            workload_shape(&fixture, "worker.execute-wasm.v1", "cases-8-evaluators-1").unwrap(),
            WorkloadShape {
                cases: 8,
                evaluators: 1,
                create_run: true,
            }
        );
        assert_eq!(
            workload_shape(&fixture, "worker.execute-wasm.v1", "cases-1-evaluators-8").unwrap(),
            WorkloadShape {
                cases: 1,
                evaluators: 8,
                create_run: true,
            }
        );
        assert_eq!(
            workload_shape(&fixture, "system.lifecycle.v1", "workers-2").unwrap(),
            WorkloadShape {
                cases: 100,
                evaluators: 1,
                create_run: false,
            }
        );
        assert_eq!(lifecycle_workers("workers-1").unwrap(), 1);
        assert_eq!(lifecycle_workers("workers-2").unwrap(), 2);
        assert_eq!(
            workload_shape(&fixture, "system.capacity.v1", "workers-2-load-16").unwrap(),
            WorkloadShape {
                cases: 1600,
                evaluators: 1,
                create_run: false,
            }
        );
        assert_eq!(lifecycle_workers("workers-1-load-4").unwrap(), 1);
        assert_eq!(lifecycle_workers("workers-2-load-8").unwrap(), 2);
        assert_eq!(
            workload_shape(&fixture, "run.create-scaling.v1", "cases-2001").unwrap(),
            WorkloadShape {
                cases: 2001,
                evaluators: 1,
                create_run: false,
            }
        );
        assert_eq!(
            workload_shape(&fixture, "coordinator.dispatch-scaling.v1", "chunks-513").unwrap(),
            WorkloadShape {
                cases: 51_300,
                evaluators: 1,
                create_run: true,
            }
        );
        assert_eq!(
            workload_shape(
                &fixture,
                "agent.http-scaling.v1",
                "cases-16-payload-1024-delay-0"
            )
            .unwrap(),
            WorkloadShape {
                cases: 16,
                evaluators: 1,
                create_run: true,
            }
        );
        assert_eq!(
            workload_shape(&fixture, "evaluator.wasm-scaling.v1", "evaluators-9").unwrap(),
            WorkloadShape {
                cases: 1,
                evaluators: 9,
                create_run: true,
            }
        );
        assert_eq!(
            parse_outbox_tuple("events-65-batch-65-parallel-8").unwrap(),
            OutboxShape {
                events: 65,
                batch: 65,
                parallel: 8,
            }
        );
        assert!(parse_outbox_tuple("events-65-batch-63-parallel-8").is_err());
        assert!(parse_single_dimension("cases-0", "cases").is_err());
        assert!(parse_http_tuple("cases-8-payload-0-delay-0").is_err());
        assert!(lifecycle_workers("workers-3-load-4").is_err());
        assert!(lifecycle_workers("workers-1-load-3").is_err());
        assert!(workload_shape(&fixture, "worker.execute-wasm.v1", "future-tuple").is_err());
        assert!(workload_shape(&fixture, "future.workload.v1", "tuple").is_err());
        assert!(lifecycle_workers("workers-3").is_err());
    }

    #[test]
    fn exact_oracles_accept_completed_workload_outcomes() {
        let create = DurableCounts::from([
            ("runs".into(), 1),
            ("chunks".into(), 11),
            ("executions".into(), 0),
        ]);
        assert!(validate_create_oracle(&create, "pending", 11).is_ok());

        assert!(validate_coordinator_oracle("running", 512, 1, 512, 512).is_ok());

        let worker = DurableCounts::from([
            ("executions".into(), 8),
            ("attempts".into(), 8),
            ("evaluator_results".into(), 8),
        ]);
        assert!(validate_worker_oracle(&worker, 1, 8, 1).is_ok());

        let evaluator_heavy = DurableCounts::from([
            ("executions".into(), 1),
            ("attempts".into(), 1),
            ("evaluator_results".into(), 8),
        ]);
        assert!(validate_worker_oracle(&evaluator_heavy, 1, 1, 8).is_ok());
        assert!(validate_lifecycle_oracle("completed", [100, 100, 100], 100).is_ok());
    }

    #[test]
    fn exact_oracles_reject_status_count_and_event_mismatches() {
        let create = DurableCounts::from([
            ("runs".into(), 1),
            ("chunks".into(), 11),
            ("executions".into(), 0),
        ]);
        assert!(validate_create_oracle(&create, "running", 11).is_err());
        assert!(validate_create_oracle(&create, "pending", 10).is_err());

        assert!(validate_coordinator_oracle("pending", 512, 1, 512, 512).is_err());
        assert!(validate_coordinator_oracle("running", 511, 1, 512, 512).is_err());
        assert!(validate_coordinator_oracle("running", 512, 0, 512, 512).is_err());
        assert!(validate_coordinator_oracle("running", 512, 1, 511, 512).is_err());

        let worker = DurableCounts::from([
            ("executions".into(), 8),
            ("attempts".into(), 8),
            ("evaluator_results".into(), 8),
        ]);
        assert!(validate_worker_oracle(&worker, 0, 8, 1).is_err());
        assert!(validate_worker_oracle(&worker, 1, 8, 2).is_err());
        assert!(validate_lifecycle_oracle("running", [100, 100, 100], 100).is_err());
        assert!(validate_lifecycle_oracle("completed", [100, 99, 100], 100).is_err());
    }

    #[test]
    fn exact_amplification_gate_accepts_match_and_rejects_drift() {
        let external = ExternalMeasurements {
            http_requests: Some(8),
            queue_ready: Some(0),
            queue_unacked: Some(0),
            durable_counts: DurableCounts::from([
                ("attempts".into(), 8),
                ("evaluator_results".into(), 8),
            ]),
            ..Default::default()
        };
        let expected = BTreeMap::from([
            ("http_requests".into(), 8),
            ("queue_ready".into(), 0),
            ("durable.attempts".into(), 8),
            ("durable.evaluator_results".into(), 8),
        ]);
        assert!(verify_exact_observations(&external, &expected).is_ok());

        let mut drift = expected.clone();
        drift.insert("http_requests".into(), 9);
        assert!(verify_exact_observations(&external, &drift).is_err());
        let unsupported = BTreeMap::from([("internal_counter".into(), 1)]);
        assert!(verify_exact_observations(&external, &unsupported).is_err());
    }
}
