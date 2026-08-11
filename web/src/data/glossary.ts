export type GlossaryEntry = {
  id: string;
  term: string;
  definition: string;
  scope: string;
};

export type GlossarySection = {
  id: string;
  entries: readonly GlossaryEntry[];
};

export const glossarySections: readonly GlossarySection[] = [
  {
    id: 'runtime-roles',
    entries: [
      {id: 'vigilo-cli', term: 'Vigilo CLI', definition: 'Command entry point for evaluator publishing, run management, shard administration, and runtime service modes.', scope: 'A command can perform work directly or run a coordinator or worker process.'},
      {id: 'coordinator', term: 'Coordinator', definition: 'Process that advances durable creation recovery, lease recovery, dispatch, finalization, and outbox publication.', scope: 'Many coordinators can run concurrently because database claims divide work.'},
      {id: 'worker', term: 'Worker', definition: 'Process that consumes chunk-ready broker messages, claims chunks, invokes the agent target and evaluators, and persists results.', scope: 'Many workers can run concurrently; a chunk claim selects one current owner.'},
      {id: 'bounded-work', term: 'Bounded work', definition: 'Work limited by a configured item count, concurrency count, time budget, or retry budget.', scope: 'Prevents one cycle, placement, chunk, or evaluator from consuming unlimited resources.'},
      {id: 'container', term: 'Container', definition: 'Independently runnable process or infrastructure service in a deployment view.', scope: 'A container is a deployment boundary, not necessarily an operating-system container.'},
      {id: 'command-flow', term: 'Command flow', definition: 'Decisions and side effects produced by one CLI command.', scope: 'One flow follows one command from input through persistence and external calls.'},
    ],
  },
  {
    id: 'evaluation-results',
    entries: [
      {id: 'agent-target', term: 'Agent target', definition: 'HTTP service or workflow whose behavior is being evaluated. It may wrap a model, prompt pipeline, or multi-step agent.', scope: 'One versioned target configuration per run profile.'},
      {id: 'run-profile', term: 'Run profile', definition: 'Versioned configuration for the agent target, evaluator bindings, scoring, persistence, retries, and gate behavior.', scope: 'One profile snapshot per run; unrelated to a Cargo build profile.'},
      {id: 'dataset', term: 'Dataset', definition: 'Versioned collection of evaluation cases.', scope: 'One dataset version per run.'},
      {id: 'case', term: 'Case', definition: 'One immutable input, optional expected output, and routing metadata from a dataset.', scope: 'A case can be evaluated once in a run.'},
      {id: 'input', term: 'Input', definition: 'Case data sent to the configured agent target.', scope: 'One required input value per dataset case.'},
      {id: 'expected-output', term: 'Expected output', definition: 'Optional oracle data available to evaluators but not sent to the agent target.', scope: 'Zero or one expected value per dataset case.'},
      {id: 'task-type', term: 'Task type', definition: 'Case label used by automatic run-profile matching.', scope: 'One task type per dataset case.'},
      {id: 'tags', term: 'Tags', definition: 'Case labels used by `tags_any` and `tags_all` profile matching rules.', scope: 'Zero or more tags per dataset case.'},
      {id: 'case-group', term: 'Case group', definition: 'Run-profile rule that selects evaluator bindings and aggregation policy for matching cases.', scope: 'One explicit group or one or more automatically matched groups per case.'},
      {id: 'evaluator', term: 'Evaluator', definition: 'Versioned WASM component that examines an agent output and emits findings.', scope: 'Identified by `<namespace>/<name>:<version>`.'},
      {id: 'evaluator-binding', term: 'Evaluator binding', definition: 'Profile entry that connects an evaluator to requiredness, a dimension, weight, blocking policy, and evaluator config.', scope: 'Many bindings can apply to one case; bindings are required by default.'},
      {id: 'evaluator-identifier', term: 'Evaluator identifier', definition: 'Immutable published identity in `<namespace>/<name>:<version>` format.', scope: 'One identifier selects one versioned evaluator artifact.'},
      {id: 'finding', term: 'Finding', definition: 'One normalized evaluator observation with status, severity, score, evidence, and blocking metadata.', scope: 'An evaluator invocation can emit multiple findings.'},
      {id: 'evaluator-completeness', term: 'Evaluator completeness', definition: 'Execution-level check that every required evaluator binding returned at least one valid score without an error or skipped finding.', scope: 'Checked before dimension and aggregate scoring; incomplete output withholds authoritative scores.'},
      {id: 'dimension', term: 'Dimension', definition: 'Profile-owned scoring bucket, such as `format` or `quality`.', scope: 'Findings are grouped into dimensions before total scoring.'},
      {id: 'dimension-score', term: 'Dimension score', definition: 'The `min_score` or `weighted_mean` result for one dimension of one execution.', scope: 'Zero or one score per configured dimension and execution.'},
      {id: 'aggregate-score', term: 'Aggregate score', definition: 'Weighted total of an execution\'s dimension scores.', scope: 'Zero or one total score per execution.'},
      {id: 'blocking-finding', term: 'Blocking finding', definition: 'Finding that can fail or error an execution independently of its aggregate score, according to profile policy.', scope: 'Blocking can come from the finding, evaluator binding, or dimension policy.'},
      {id: 'execution', term: 'Execution', definition: 'Durable evaluation of one dataset case against the agent target.', scope: 'One expected execution per case in a run.'},
      {id: 'attempt', term: 'Attempt', definition: 'One worker\'s effort to complete an execution. Retries create later attempts.', scope: 'Many attempts can belong to one execution; only the current attempt is authoritative.'},
      {id: 'current-attempt', term: 'Current attempt', definition: 'Attempt ID and number that an execution currently authorizes to write terminal state.', scope: 'Zero or one authoritative attempt per execution.'},
      {id: 'run', term: 'Run', definition: 'One durable evaluation of a dataset version with a run profile and agent target.', scope: 'A run contains chunks and expected executions.'},
      {id: 'run-status', term: 'Run status', definition: 'Operational lifecycle state such as `creating`, `pending`, `running`, `completed`, `failed`, or `cancelled`.', scope: 'One current value per run.'},
      {id: 'gate-status', term: 'Gate status', definition: 'Policy outcome such as `unknown`, `pass`, or `fail`.', scope: 'One current value per run; distinct from run status.'},
    ],
  },
  {
    id: 'work-routing',
    entries: [
      {id: 'chunk', term: 'Chunk', definition: 'Bounded range of dataset cases processed under one worker claim.', scope: 'Many chunks per run; each chunk belongs to one run shard.'},
      {id: 'in-flight-chunk', term: 'In-flight chunk', definition: 'Chunk-ready broker delivery currently being processed by one worker process.', scope: 'Bounded per worker process by `max_inflight_chunks`.'},
      {id: 'prefetch', term: 'Prefetch', definition: 'RabbitMQ limit on unacknowledged deliveries reserved by one consumer.', scope: 'Configured per worker consumer.'},
      {id: 'chunk-parallelism', term: 'Chunk parallelism', definition: 'Number of case executions processed concurrently inside one claimed chunk.', scope: 'Bounded independently from in-flight chunk count.'},
      {id: 'run-shard', term: 'Run shard', definition: 'Stable logical segment numbered `0..127` and stored as `run_shard`; it keeps a chunk and its execution-owned rows together.', scope: 'A run uses only the shards assigned to its chunks.'},
      {id: 'control-database', term: 'Control database', definition: 'PostgreSQL role that owns global run state, placement metadata, dispatch cursors, creation plans, and control outbox records.', scope: 'Exactly one active control-capable database placement.'},
      {id: 'execution-database', term: 'Execution database', definition: 'PostgreSQL role that owns shard-local chunks, snapshots, executions, attempts, results, summaries, and chunk-ready outbox records.', scope: 'One or more shard-capable placements; the control database may also serve this role.'},
      {id: 'database-alias', term: 'Database alias', definition: 'Stable name such as `primary` or `shard_001` used instead of a connection URL.', scope: 'One alias per database placement.'},
      {id: 'database-placement', term: 'Database placement', definition: 'Catalog entry that maps a database alias to a secret environment-variable name, role, and status.', scope: 'One row per configured PostgreSQL target.'},
      {id: 'database-placement-status', term: 'Database placement status', definition: 'Admission lifecycle for a target: `active` accepts and serves ownership, `draining` serves existing ownership but accepts none, and `disabled` serves no runtime work.', scope: 'One status per database placement.'},
      {id: 'placement-drain', term: 'Placement drain', definition: 'Guarded transition that stops new shard ownership before routes are moved away and a database placement is disabled.', scope: 'The drain does not move rows by itself.'},
      {id: 'database-router', term: 'Database router', definition: 'Process-local `DatabaseRouter` that reads placement metadata and resolves control or execution pools.', scope: 'One lazily initialized router per Vigilo process; it does not choose new shard assignments.'},
      {id: 'database-circuit-breaker', term: 'Database circuit breaker', definition: 'Process-local admission guard that temporarily skips one unavailable database alias without changing durable routing.', scope: 'One independent circuit per contacted execution database alias and process.'},
      {id: 'shard-placement', term: 'Shard placement', definition: 'Control-plane mapping from `run_id + run_shard` to a database alias, lifecycle, route version, and write epoch.', scope: 'One row per used run shard.'},
      {id: 'execution-route', term: 'Execution route', definition: 'Resolved shard placement plus the PostgreSQL pool for its current database alias.', scope: 'Resolved for one `run_id + run_shard`.'},
      {id: 'route-version', term: 'Route version', definition: 'Monotonically increasing control-plane CAS generation changed by every route alias or lifecycle update.', scope: 'One current value per shard placement; not a schema or deployment version.'},
      {id: 'write-epoch', term: 'Write epoch', definition: 'Monotonically increasing execution-ownership generation carried by routed work and validated in the destination database.', scope: 'Changes only when ownership moves or is restored.'},
      {id: 'local-shard-admission', term: 'Local shard admission', definition: 'Execution-database authority row containing the accepted write epoch and `open`, `draining`, `prepared`, or `closed` state.', scope: 'One row per locally known `run_id + run_shard`; checked in the write transaction.'},
      {id: 'dispatch-cursor', term: 'Dispatch cursor', definition: 'Control-database progress for dispatching one run shard.', scope: 'One cursor per used run shard after creation; `drained` forbids further dispatch.'},
      {id: 'chunk-dispatch-window', term: 'Chunk dispatch window', definition: 'Bounded set of pending chunks selected from one run shard in one dispatch operation.', scope: 'One window can create one `run.chunk.ready` outbox event record per selected chunk.'},
      {id: 'coordinator-cycle', term: 'Coordinator cycle', definition: 'Ordered iteration of creation recovery, lease recovery, chunk dispatch, finalization, and outbox publication.', scope: 'Repeats for `coordinator start`; runs once for `coordinator once`.'},
      {id: 'coordinator-pass', term: 'Coordinator pass', definition: 'One bounded stage within a coordinator cycle, such as dispatch or outbox publication.', scope: 'A pass can visit multiple database aliases.'},
      {id: 'shard-move', term: 'Shard move', definition: 'Targeted relocation of one `run_id + run_shard` route and its shard-owned rows to another database alias.', scope: 'One run shard per move operation.'},
      {id: 'rebalance-plan', term: 'Rebalance plan', definition: 'Persisted set of targeted shard moves for a capacity or placement-drain operation.', scope: 'One plan contains many rebalance items.'},
      {id: 'rebalance-item', term: 'Rebalance item', definition: 'Claimable plan item for moving one specific `run_id + run_shard`.', scope: 'One shard move per item; concurrent apply processes can claim different items.'},
    ],
  },
  {
    id: 'ownership-concurrency',
    entries: [
      {id: 'state', term: 'State', definition: 'Persisted lifecycle value used to determine which transitions are valid.', scope: 'One current lifecycle value per stateful record.'},
      {id: 'transition', term: 'Transition', definition: 'Guarded database change from one state to another.', scope: 'A transition applies only when its authority and current-state predicates hold.'},
      {id: 'owner', term: 'Owner', definition: 'Process or claim that currently has guarded authority to perform a state transition.', scope: 'Ownership is temporary unless represented by durable placement state.'},
      {id: 'claim', term: 'Claim', definition: 'Successful transition that gives a process temporary authority over one work item.', scope: 'Examples include chunk, dispatch-cursor, outbox-delivery, and rebalance-item claims.'},
      {id: 'lease', term: 'Lease', definition: 'Time-bounded claim authority that becomes recoverable after its deadline.', scope: 'Expiry permits recovery but does not alone prevent a stale write.'},
      {id: 'claim-token', term: 'Claim token', definition: 'Opaque value issued with a claim and required to settle or renew that exact claim.', scope: 'A newer claim gets a different token, fencing the previous owner.'},
      {id: 'fencing-token', term: 'Fencing token', definition: 'Value whose equality proves that an owner or route is still current.', scope: 'Checked on every protected mutation; claim tokens and route versions are fencing values.'},
      {id: 'row-lock', term: 'Row lock', definition: 'PostgreSQL lock on selected table rows, usually held until the transaction ends.', scope: 'PostgreSQL enforces conflicting row-lock modes.'},
      {id: 'admission-lock', term: 'Admission lock', definition: 'Transaction-scoped PostgreSQL advisory lock used cooperatively by shard writers.', scope: 'Normal writers take the shared form; shard movement takes the exclusive form for one `run_id + run_shard`.'},
      {id: 'route-fence', term: 'Route fence', definition: 'Expected database alias, placement status, and route version validated immediately before a routed write.', scope: 'One expected fence per resolved execution route.'},
      {id: 'route-cas', term: 'Route CAS', definition: 'Compare-and-swap update that changes a route only while its stored alias, lifecycle, and route version still match.', scope: 'Serializes concurrent control-plane route changes.'},
      {id: 'compare-and-swap', term: 'Compare-and-swap', definition: 'Update that succeeds only if stored values still equal expected values.', scope: 'Used to change a route or settle a claim without overwriting newer state.'},
      {id: 'idempotent-operation', term: 'Idempotent operation', definition: 'Operation that can be repeated without duplicating its logical effect.', scope: 'Retries may execute more than once while producing one durable outcome.'},
      {id: 'no-op', term: 'No-op', definition: 'Valid path that changes nothing because another process already advanced the state.', scope: 'A no-op is a successful convergence outcome, not necessarily an error.'},
      {id: 'recovery', term: 'Recovery', definition: 'Reassignment or repair after a lease expires or a durable workflow stops mid-operation.', scope: 'Recovery preserves retry limits and invalidates stale owners.'},
      {id: 'stale-attempt', term: 'Stale attempt', definition: 'Attempt that is no longer authoritative because its lease expired, its chunk was recovered, or a later attempt superseded it.', scope: 'Retained for history but rejected as a current writer.'},
    ],
  },
  {
    id: 'events-messaging',
    entries: [
      {id: 'outbox-event-record', term: 'Outbox event record', definition: 'Durable `outbox_events` row inserted in the same database transaction as the state change it describes.', scope: 'One logical event per unique `dedupe_key`.'},
      {id: 'outbox-delivery-row', term: 'Outbox delivery row', definition: 'Temporary publish work in `outbox_delivery_queue`.', scope: 'One active delivery row per unpublished outbox event record.'},
      {id: 'publish-claim', term: 'Publish claim', definition: 'Time-bounded ownership of an outbox delivery row, fenced by `claim_token`.', scope: 'One current publisher claim per delivery row.'},
      {id: 'broker', term: 'Broker', definition: 'RabbitMQ transport that carries published messages from coordinators to workers.', scope: 'Transport is at-least-once; durable authority remains in PostgreSQL.'},
      {id: 'broker-message', term: 'Broker message', definition: 'RabbitMQ message created from an outbox event record after publication.', scope: 'May be delivered more than once.'},
      {id: 'worker-delivery', term: 'Worker delivery', definition: 'One broker delivery that identifies a chunk for a worker to claim.', scope: 'A delivery is acknowledged, delayed, requeued, or quarantined after processing.'},
      {id: 'event-type', term: 'Event type', definition: 'Semantic event name such as `run.started`, `run.chunk.ready`, or `run.completed`.', scope: 'Stored on the outbox record and used for broker routing.'},
      {id: 'dedupe-key', term: 'Dedupe key', definition: 'Stable identity for one logical outbox event record, also published as the AMQP message ID.', scope: 'Unique in the outbox ledger.'},
      {id: 'at-least-once-delivery', term: 'At-least-once delivery', definition: 'Delivery guarantee that permits redelivery after uncertain acknowledgement.', scope: 'Consumers must use database claims and idempotency guards.'},
      {id: 'publisher-confirm', term: 'Publisher confirm', definition: 'RabbitMQ acknowledgement that a published message reached the broker.', scope: 'Required before the outbox event record is marked published.'},
      {id: 'message-settlement', term: 'Message settlement', definition: 'Worker acknowledgement, delayed redelivery, or requeue decision for one broker delivery.', scope: 'One settlement outcome per received delivery.'},
    ],
  },
  {
    id: 'evaluator-packaging',
    entries: [
      {id: 'wit', term: 'WIT', definition: 'WebAssembly Interface Type definition used as the evaluator ABI source of truth.', scope: '`wit/evaluator.wit` defines canonical evaluator `input` and `output`.'},
      {id: 'wit-world', term: 'WIT world', definition: 'WIT boundary that groups the evaluator\'s imported and exported interfaces.', scope: 'The evaluator implements `evaluator-world`.'},
      {id: 'wasi-preview-2', term: 'WASI Preview 2', definition: 'Component-oriented WASI target used by evaluator artifacts.', scope: 'Rust target `wasm32-wasip2`.'},
      {id: 'evaluator-artifact', term: 'Evaluator artifact', definition: 'Compiled WebAssembly component stored in the evaluator registry.', scope: 'One immutable artifact content per evaluator identifier.'},
      {id: 'evaluator-registry', term: 'Evaluator registry', definition: 'Durable catalog of published evaluator identities, metadata, contracts, and artifact content.', scope: 'Contains many versioned evaluators.'},
      {id: 'package-manifest', term: 'Package manifest', definition: '`Cargo.toml` file that supplies evaluator crate identity and version.', scope: 'One Cargo manifest per evaluator crate.'},
      {id: 'evaluator-manifest', term: 'Evaluator manifest', definition: '`Vigilo.toml` file describing artifact paths, WIT expectations, and publish metadata.', scope: 'One per evaluator package.'},
      {id: 'wit-contract', term: 'WIT contract', definition: 'Interface definition that a compiled evaluator component must implement.', scope: 'Validated against the configured WIT package, world, interface, and version.'},
      {id: 'build-profile', term: 'Build profile', definition: 'Named artifact selection in `Vigilo.toml`, such as `dev` or `release`.', scope: 'Selects a build output; not an evaluation run profile.'},
      {id: 'wasm-store', term: 'Wasm store', definition: 'Fresh Wasmtime execution state created for one evaluator invocation.', scope: 'One isolated store per invocation.'},
      {id: 'fuel', term: 'Fuel', definition: 'Deterministic Wasmtime instruction budget for one evaluator invocation.', scope: 'Exhaustion interrupts evaluator execution.'},
      {id: 'evaluator-semaphore', term: 'Evaluator semaphore', definition: 'Process-local cap on concurrently active Wasm evaluator invocations.', scope: 'One shared semaphore per worker process.'},
      {id: 'publish', term: 'Publish', definition: 'Validate and insert a versioned evaluator artifact into the evaluator registry.', scope: 'Publishing is immutable for one evaluator identifier.'},
    ],
  },
] as const;

function indexById<T extends {id: string}>(values: readonly T[], label: string): Map<string, T> {
  const index = new Map<string, T>();
  for (const value of values) {
    if (index.has(value.id)) {
      throw new Error(`Duplicate ${label} ID: ${value.id}`);
    }
    index.set(value.id, value);
  }
  return index;
}

export const glossaryEntries = indexById(
  glossarySections.flatMap((section) => section.entries),
  'glossary entry',
);

export const glossarySectionById = indexById(glossarySections, 'glossary section');
