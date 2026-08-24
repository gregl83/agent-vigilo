# Vigilo Performance Harness

`cargo perf` is a repository-local Cargo alias backed by the `xtask` workspace
package. It is not installed globally and it is not shipped with Vigilo.

The harness includes component scaling models, canonical noise analysis,
bounded one/two-worker capacity calibration, reviewed budget publication, and
confirmed regression gates. Startup remains service-free; run creation,
coordinator, HTTP agent, worker/Wasm, persistence, and lifecycle measurements
use a fresh run-owned PostgreSQL database clone, RabbitMQ vhost/namespace, and
deterministic HTTP agent for every sample.
Cancellation, terminal reads, exports, shard movement, rebalance, and logical
placement workloads add routed database clones only when their selected tuple
requires them. Their setup remains outside the measured process boundary.

## Module Overview

- `build` snapshots the release executable and workspace-independent setup
  assets, then records their digests and supported CLI capabilities.
- `config` and `model` own typed registry, profile, sample, comparison,
  calibration, scaling, and diagnostic contracts.
- `workload`, `fixture`, and `service` prepare isolated databases and broker
  scopes, run supported release commands, collect external observations, and
  enforce exact oracles.
- `process` bounds and reaps process trees; `schedule` and `stats` implement
  counterbalanced sampling and comparison estimates.
- `scaling` accepts fixed-plus-slope or explicit stepped models only from
  repeated valid samples; `diagnostics` renders non-gating PostgreSQL statement,
  planning, buffer, and WAL evidence captured after process timing.
- `calibration`, `command`, `report`, `artifact`, and `check` analyze evidence,
  orchestrate campaigns, persist artifacts, render results, and enforce
  repository isolation.

## Layout And Configuration

- `performance/registry/workloads-v1.toml` defines the available workload
  contracts: fixture tuples, correctness oracles, required metrics, limits, and
  implementation status.
- `performance/profiles/*.toml` select workload tuples and define block counts,
  scheduling, timing policy, and campaign limits.
- `performance/environments/*.toml` describe hosts on which results may be
  considered comparable.
- `performance/fixtures/*.toml` define deterministic logical input shapes.
- `performance/compose.yml` defines the private PostgreSQL and RabbitMQ
  topology created for one campaign.
- `xtask` implements `cargo perf`; generated builds and results stay under
  `target/perf`.

The `performance` directory is for the external process and service harness.
Criterion targets are added under `vigilo/benches` only when production already
has a stable library surface worth measuring. Vigilo currently keeps runtime,
database, and CLI modules binary-private, so there is no Criterion target and no
API is widened solely for benchmarking.

See [`schemas/README.md`](schemas/README.md) for the registry, profile,
environment, and generated-artifact field reference.

## Profiles

| Profile | Purpose |
| --- | --- |
| `developer-v1` | Fast local diagnosis of explicitly selected workloads using two balanced blocks. |
| `pr-v1` | Pull-request correctness checks and short, informative timing canaries. |
| `reference-v1` | Broader repeatable comparison across the complete MVP workload matrix. |
| `calibration-v1` | Stable no-change campaign used to measure canonical-host and harness noise. |
| `capacity-v1` | Bounded one/two-worker load staircase, separate from fixed-load comparisons. |
| `component-smoke-v1` | Explicitly selected, small exact-oracle checks for each component driver. |
| `component-reference-v1` | Repeated samples at every registered model point and batching boundary. |
| `component-nightly-v1` | Broader orthogonal payload, latency, and large-cardinality diagnostics. |
| `admin-smoke-v1` | Explicitly selected exact-oracle checks for routed administration and large-data drivers. |
| `admin-nightly-v1` | Scheduled cancellation, read/export, movement, placement, and creation-boundary evidence. |

`reference-v2` is generated only after a canonical calibration passes review.
It references a versioned budget policy and contains only tuples whose block
counts and wall-time budgets have calibration evidence. Generated candidates
remain under `target/perf/baselines` until their profile and budget files are
reviewed and checked into `performance/`.

Profiles configure campaigns; they do not implement workloads. A selection
fails before provisioning if its workload, tuple, fixture, or binary capability
is unavailable.

Calibration compares identical immutable builds under a stable workload and
sampling configuration. Because the expected product difference is zero, its
observed A/B effects estimate normal environmental and measurement variation.
`cargo perf calibrate noise` converts the comparison into repeatability,
orientation, noise-bound, estimated-power, and recommended-block evidence. The
block recommendation uses the observed 95% bootstrap interval width and the
reviewed power target; it is an approximation over independent blocks, not a
claim that executions inside one block are independent. Reviewed practical
budgets come from `performance/budgets/review-targets-v1.toml`; calibration
validates that the host can resolve those budgets and never raises them to make
noise pass.

## Quick Start

Validate the harness, package boundary, profiles, scheduler, and process cleanup:

```bash
cargo perf check
```

Build an immutable release snapshot and provenance manifest:

```bash
cargo perf build --source . --output target/perf/builds/current
```

A clean production release can take several minutes because Vigilo enables LTO
and one codegen unit. Build snapshots once per revision and reuse them.
`cargo perf build` also compiles and snapshots the frozen sentiment evaluator;
`run` and `compare` never invoke Cargo inside a measured campaign.

On Linux, the binary is `target/perf/builds/current/release/vigilo`. On Windows,
append `.exe`. Measure one binary:

```bash
cargo perf run \
  --profile developer-v1 \
  --workload startup.cli-help.v1 \
  --bin <vigilo-binary> \
  --build-manifest target/perf/builds/current/build-manifest.json
```

Compare two immutable snapshots on the same host:

```bash
cargo perf compare \
  --profile developer-v1 \
  --workload startup.cli-help.v1 \
  --baseline-bin <baseline-vigilo-binary> \
  --baseline-build-manifest <baseline-build-manifest.json> \
  --candidate-bin <candidate-vigilo-binary> \
  --candidate-build-manifest <candidate-build-manifest.json>
```

Each comparison uses two counterbalanced blocks: one `ABBA` and one `BAAB`, for
eight measured executions. Readiness and the one-per-binary startup
preconditioning runs are recorded separately and excluded from measurements.
The schedule seed and logical block index make position order reproducible.

Select any workload present in the chosen profile. Docker is provisioned
lazily, so startup-only campaigns do not start services.

Fit the registered component models after a complete component reference run:

```bash
cargo perf model --run-dir target/perf/runs/<component-run>
```

The command writes `component-models.json` and rejects missing repetitions,
exact-count drift, negative coefficients, or residuals above the registered
tolerance. Render the post-timing database evidence separately:

```bash
cargo perf diagnose --run-dir target/perf/runs/<component-run>
```

`diagnostics.md` is investigative output and never changes a correctness,
comparison, budget, or model verdict.

## Calibration And Baselines

Publishable campaigns require the `aws-m6i-2xlarge-al2023-v1` Linux host contract
and an externally validated `VIGILO_PERF_CANONICAL_VALIDATED=1` declaration.
Do not set that declaration on local or shared CI hosts. Local runs may produce
informative diagnostics, but publication rejects their evidence.

Run `calibration-v1` as a same-build comparison, then analyze it:

```bash
cargo perf compare --profile calibration-v1 \
  --baseline-bin <vigilo-binary> --baseline-build-manifest <build-manifest.json> \
  --candidate-bin <same-vigilo-binary> --candidate-build-manifest <same-build-manifest.json>
cargo perf calibrate noise --run-dir target/perf/runs/<calibration-run>
```

Run the separately bounded capacity staircase for the same build:

```bash
cargo perf run --profile capacity-v1 \
  --bin <vigilo-binary> --build-manifest <build-manifest.json>
cargo perf calibrate capacity --run-dir target/perf/runs/<capacity-run>
```

Capacity steps use one and two workers at predeclared load multipliers
`1`, `2`, `4`, `8`, and `16`. The analyzer identifies the first step with less
than 10% throughput gain and more than 25% p95 terminal-latency growth, or 90%
normalized per-worker process CPU. Shared-service CPU at or above 85%
invalidates the staircase instead of being labeled as worker capacity. When no
knee appears, the artifact records only an observed rate lower bound.

After review, publish an immutable candidate baseline:

```bash
cargo perf calibrate publish \
  --calibration target/perf/runs/<calibration-run>/calibration.json \
  --capacity target/perf/runs/<capacity-run>/capacity.json \
  --build-manifest <build-manifest.json> \
  --approved-by <review-identity>
```

Publication refuses mismatched build digests, unrepeatable noise, invalid
capacity evidence, and an existing output directory. It emits digested evidence,
`reference-v2`, its budget policy, and the frozen build manifest beneath
`target/perf/baselines/reference-v2`.

## MVP Workloads

| Workload | Measured region | Exact postcondition |
| --- | --- | --- |
| `startup.cli-help.v1` | One release process executing `--help` | Exit `0` and frozen help signatures. |
| `run.create.v1` | `run create` for 1,001 cases | One pending run, 1,001 case memberships, zero executions, and 11 chunks. |
| `coordinator.dispatch.v1` | One `coordinator once` cycle | One legal start and all 512 chunks dispatched to the run-owned broker scope. |
| `worker.execute-wasm.v1` | One `worker once` pass | Exact attempts/results for either 8x1 or 1x8, one completed chunk, and drained delivery. |
| `system.lifecycle.v1` | Create through terminal state with one or two worker processes | One completed passing run, exact execution/result counts, and drained delivery. |

## Component Workloads

The component registry crosses selected production discontinuities without
generating a Cartesian product:

| Family | Registered boundaries | Exact observations |
| --- | --- | --- |
| Run creation | Case/page/group points from 1 through 10,000 | Runs, chunks, cases, and zero premature executions. |
| Dispatch | 1, 512, and 513 chunks | Dispatched chunks, durable events, and worker deliveries. |
| HTTP agent | 1, 8, and 16 requests; 1/64 KiB payloads; 0/10 ms delay | Requests, bytes, peak concurrency, attempts, and results. |
| Wasm and persistence | Evaluators 1/8/9 and cases 1/8/9/100 | Attempts, evaluator results, HTTP calls, and drained work. |
| Outbox | Batches 1/64/65/256/1,000 with parallelism 1/8/64 | Exact published worker deliveries and durable event rows. |
| Recovery | Expired leases 1/1,000/1,001 | Recovery counts, recovery events, and redeliveries. |
| Finalization | Terminal runs 1/64/65 | Completed runs, evaluator results, and completion events. |
| Cancellation | 1/4/17 logical routes and 1,024/8,192 open executions | Exact cancelled rows, terminal state, and idempotent replay. |
| Status and results | 250/251 terminal executions over one/two routes | Completed status plus exact execution, result, and diagnostic counts. |
| JSON and JSONL export | 250/251 terminal executions with a 250-row page | Exact record types/counts, output bytes, first/last byte, and peak RSS. |
| Shard move | 1,000/1,001 narrow rows and payloads around 4 MiB | Verified route switch, idempotent replay, and actual per-table rows, bytes, and page counts. |
| Rebalance | One/eight persisted items across two databases | One claimed item per resumable apply pass, exact verification, and every route on the target. |
| Placement | 1/8/16/32 logical placements on one PostgreSQL server | Exact bounded dispatch work and a fixed-cost-plus-placement slope. |
| Creation limits | Both sides of grouping and the 64-page handoff | Exact pending or recoverable-creating state and materialized case count. |

The router/cache contract remains registered but unavailable because the
release CLI cannot keep its process-local cache alive across calls. Adding a
benchmark-only command or exposing binary-private modules would measure a new
path rather than the shipped one, so selection fails closed until a legitimate
reusable production boundary exists.

Migration, evaluator publication, fixture creation, database cloning, and
worker queue preparation occur before collector reset and are excluded from
measurement. Lifecycle intentionally includes creation and every runtime
process.

Routed samples reset and aggregate PostgreSQL statement counts, rows, database
growth, and diagnostics across every participating database. Cluster WAL is
counted once because the reference topology uses one PostgreSQL server. JSONL
must remain below its 256 MiB peak-RSS contract at the registered page boundary;
JSON intentionally materializes one document and has a separate 512 MiB tested
limit. These limits are correctness guards for the registered fixture size, not
calibrated regression budgets or production capacity claims.

## Performance Test Tier

The service ownership/reset/cleanup integration test is explicit and opt-in:

```bash
cargo test -p xtask --locked --features performance-services --test performance_services -- --nocapture
```

It verifies a collector sentinel is visible once, absent after reset in the
next sample, and followed by exact resource cleanup. It is not part of normal
unit, PostgreSQL, migration, or end-to-end tiers.

## Results

Run output is written only below `target/perf/runs`. Each directory contains:

- `campaign.json` and `environment.json` for execution provenance
- `readiness.jsonl`, `samples.jsonl`, and `blocks.jsonl` for raw evidence
- `comparisons.jsonl` and per-workload comparison JSON for A/B results
- `report.json` as the report contract
- `summary.md` as the concise human-readable result
- `component-models.json` after `cargo perf model`
- `diagnostics.md` after `cargo perf diagnose`

Use `cargo perf report --run-dir <run-directory>` to regenerate the terminal and
Markdown views from `report.json`.

Exit `0` means all required correctness checks passed. Exit `1` means a
candidate crash, timeout, output overflow, or exact-oracle failure. Exit `2`
means the campaign was invalid, unsupported, inconclusive, or incomplete.
Timing remains informative in profiles without a reviewed budget; calibrated
numerical regression budgets apply only through a published gating profile. An initial
over-budget confidence interval exits `2`; repeat it with
`--confirmation-of <prior-run-directory>`. The confirmation must match the
profile, workload set, build digests, and canonical environment and uses an
independent schedule seed. A second over-budget interval exits `1`.

## Isolation

- `xtask` has no dependency on the `vigilo` crate.
- `vigilo` has no normal or build dependency on performance tooling.
- The startup workload uses only the supported release executable and `--help`.
- Service workloads accept only dynamically assigned loopback endpoints.
- Each campaign gets a unique Compose project, network, labelled volumes,
  database-name prefix, RabbitMQ vhost, and queue namespace.
- Build snapshots include digested migrations, WIT, evaluator metadata, and
  evaluator Wasm; the harness never mutates the source copies.
- Process output, time, and artifacts are bounded. Timeout cleanup targets the
  Windows Job Object or Unix process group and always reaps the child.
- A workspace host lease prevents release builds and measurement campaigns from
  overlapping and contaminating one another; a stale owner is recovered by PID.

Teardown compares the live container and volume inventory to `services.json`
and rechecks live ownership labels. A mismatch refuses destructive cleanup;
only the exact recorded resources can be removed.
