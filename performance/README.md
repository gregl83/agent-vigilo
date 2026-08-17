# Vigilo Performance Harness

`cargo perf` is a repository-local Cargo alias backed by the `xtask` workspace
package. It is not installed globally and it is not shipped with Vigilo.

Phase 1 implements the service-free `startup.cli-help.v1` workload. The run
creation, coordinator, worker/Wasm, and lifecycle contracts are registered but
remain unavailable until their isolated services and fixtures land in Phase 2.
The runner fails an unavailable selection instead of skipping it.

## Layout And Configuration

- `performance/registry/workloads-v1.toml` defines the available workload
  contracts: fixture tuples, correctness oracles, required metrics, limits, and
  implementation status.
- `performance/profiles/*.toml` select workload tuples and define block counts,
  scheduling, timing policy, and campaign limits.
- `performance/environments/*.toml` describe hosts on which results may be
  considered comparable.
- `xtask` implements `cargo perf`; generated builds and results stay under
  `target/perf`.

The `performance` directory is for the external process and service harness.
Rust-native microbenchmarks belong in Cargo's conventional `vigilo/benches`
directory and are a separate measurement tier.

See [`schemas/README.md`](schemas/README.md) for the registry, profile,
environment, and generated-artifact field reference.

## Profiles

| Profile | Purpose |
| --- | --- |
| `developer-v1` | Fast local diagnosis of explicitly selected workloads using two balanced blocks. |
| `pr-v1` | Pull-request correctness checks and short, informative timing canaries. |
| `reference-v1` | Broader repeatable comparison across the complete MVP workload matrix. |
| `calibration-v1` | Stable no-change campaign used to measure canonical-host and harness noise. |

Profiles configure campaigns; they do not implement workloads. Phase 1 can run
only `startup.cli-help.v1`, so any selected profile containing a planned
workload exits as unsupported rather than producing a partial result.

Calibration compares identical immutable builds under a stable workload and
sampling configuration. Because the expected product difference is zero, its
observed A/B effects estimate normal environmental and measurement variation.
Phase 3 uses repeated calibration results to choose block counts, host-validity
limits, and practical regression budgets. Calibration results are currently
reviewed manually; `timing = "calibration"` does not yet calculate or publish
those limits automatically.

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
and one codegen unit. Build snapshots once per revision and reuse them: `run`
and `compare` never invoke Cargo inside a measured campaign.

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

## Results

Run output is written only below `target/perf/runs`. Each directory contains:

- `campaign.json` and `environment.json` for execution provenance
- `readiness.jsonl`, `samples.jsonl`, and `blocks.jsonl` for raw evidence
- `comparisons.jsonl` and per-workload comparison JSON for A/B results
- `report.json` as the report contract
- `summary.md` as the concise human-readable result

Use `cargo perf report --run-dir <run-directory>` to regenerate the terminal and
Markdown views from `report.json`.

Exit `0` means all required Phase 1 correctness checks passed. Exit `1` means a
candidate crash, timeout, output overflow, or exact-oracle failure. Exit `2`
means the campaign was invalid, unsupported, inconclusive, or incomplete.
Timing remains informative in the Phase 0 profiles; calibrated numerical
regression budgets arrive in Phase 3 rather than being invented from local
noise.

## Isolation

- `xtask` has no dependency on the `vigilo` crate.
- `vigilo` has no normal or build dependency on performance tooling.
- The startup workload uses only the supported release executable and `--help`.
- No PostgreSQL, RabbitMQ, HTTP agent, Docker network, or persistent volume is
  accessed in Phase 1.
- Build snapshots include digested migrations and WIT setup assets, but the
  harness never mutates the source copies.
- Process output, time, and artifacts are bounded. Timeout cleanup targets the
  Windows Job Object or Unix process group and always reaps the child.
- A workspace host lease prevents release builds and measurement campaigns from
  overlapping and contaminating one another; a stale owner is recovered by PID.

Phase 2 will add a standalone run-scoped service topology below `target/perf`.
It will not attach to development databases, vhosts, networks, or volumes.
