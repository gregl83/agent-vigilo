[![Build](https://github.com/gregl83/agent-vigilo/actions/workflows/build.yaml/badge.svg?branch=main)](https://github.com/gregl83/agent-vigilo/actions/workflows/build.yaml)
[![Coverage Status](https://codecov.io/gh/gregl83/agent-vigilo/graph/badge.svg?token=CL93O7DW9C)](https://codecov.io/gh/gregl83/agent-vigilo)
[![Crates.io](https://img.shields.io/crates/v/agent-vigilo.svg)](https://crates.io/crates/agent-vigilo)
[![Documentation](https://img.shields.io/badge/docs-agentvigilo.com-blue.svg)](https://agentvigilo.com/docs/guides/getting-started)
[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

<p align="center">
  <img src="web/static/img/logo.svg" alt="Agent Vigilo logo" width="120" />
</p>

# Agent Vigilo

**Distributed AI evaluation infrastructure and deployment gating experiments for generative AI systems.**

Agent Vigilo explores what LLM and agent evaluation infrastructure can look like beyond ad hoc scripts: versioned WASM evaluators, durable evaluation runs, worker/coordinator execution, normalized results, and pass/fail gates that can sit in CI or release workflows.

It focuses on the parts of AI evaluation that become hard as systems grow: idempotent distributed work, durable event delivery, evaluator isolation, retry-safe persistence, and auditable results.

## Why It Matters

- **Run evaluations like infrastructure**: PostgreSQL-backed state, RabbitMQ work distribution, Rust workers, and deterministic state guards.
- **Ship versioned evaluators**: publish WASI Preview 2 WebAssembly evaluators with strict WIT contracts.
- **Protect the runtime**: Wasmtime fuel, memory, timeout, log, and concurrency limits isolate evaluator execution.
- **Avoid lost events**: durable outbox ledger plus hot delivery queue, RabbitMQ publisher confirms, and idempotency keys.
- **Gate deployments**: turn evaluator measurements and host-owned profile policy into dimension scores, total aggregate scores, and reproducible release decisions.

## How Results Are Calculated

Each evaluator invocation returns one measurement or abstention plus optional diagnostics. The profile binding owns normalization, threshold, dimension, weight, requiredness, and blocking policy. Missing, errored, abstained, duplicated, or invalid required output produces an errored execution aggregate with no score. An execution passes when completeness is satisfied, `aggregate_score >= min_execution_score`, and no host-derived blocking result fails.

A run can fail operationally because work did not complete, or complete with a failed gate because evaluation policy failed.

## Start Here

- [Getting started](https://agentvigilo.com/docs/guides/getting-started): run your first evaluation.
- [CLI reference](https://agentvigilo.com/docs/guides/cli-reference): canonical commands for evaluators, runs, databases, and rebalancing.
- [Architecture overview](https://agentvigilo.com/docs/architecture/structure/): containers, components, flows, and state diagrams.
- [Scale-out and shard migration](https://agentvigilo.com/docs/architecture/scaling): 128 logical run shards and expansion guidance.
- [Worker runtime](https://agentvigilo.com/docs/architecture/structure/components/worker/): chunk claiming, evaluator execution, and result persistence.
- [Runtime limits](https://agentvigilo.com/docs/configuration/runtime-limits): Wasm evaluator sandbox and worker concurrency controls.
- [Outbox lifecycle](https://agentvigilo.com/docs/architecture/state/outbox-lifecycle/): durable event publication and retry behavior.
- [Publishing evaluators](https://agentvigilo.com/docs/guides/publishing): build and publish versioned WASM evaluators.

## Core Stack

Rust, Tokio, PostgreSQL, SQLx, RabbitMQ, Wasmtime, WASI Preview 2, WIT, Docusaurus.

## Development Checks

GitHub Actions is the source of truth for build verification. To install the optional local Git hooks:

```bash
chmod +x scripts/hooks/pre-commit scripts/hooks/pre-push
git config core.hooksPath scripts/hooks
```

### Test Tiers

| Tier | Scope | Required services | GitHub Actions job |
| --- | --- | --- | --- |
| Unit and contract | Service-free Rust behavior across the workspace | None | `Unit and Contract Tests` |
| Database integration | SQL, migrations, transactions, advisory locks, leases, and concurrency | One PostgreSQL server; `DATABASE_URL` must use a role that can create test databases | `Database Integration Tests` |
| Migration | Greenfield schema application through the CLI setup path | One fresh PostgreSQL database | `Migration Tests` |
| End-to-end | Routing and the real distributed runtime across process and protocol boundaries | Two PostgreSQL servers, RabbitMQ, evaluator Wasm, and the test HTTP agent | `End-to-End Tests` |

Run the service-free tier with:

```bash
cargo test --workspace --locked --lib --bins
```

After setting `DATABASE_URL`, run every PostgreSQL-backed SQLx test with:

```bash
cargo test -p vigilo --locked --bin vigilo -- --ignored --nocapture --test-threads=4
```

Within the `vigilo` binary target, `#[ignore]` is reserved for this database
integration tier so the command remains complete.

The end-to-end tier runs `multi_database_routing` and `multi_database_e2e`
separately because they require the complete distributed dependency set. The
workflow builds the evaluator Wasm and supplies the required database,
messaging, and opt-in environment variables.

The pre-commit hook runs nightly rustfmt only. The pre-push hook runs clippy, the service-free Rust tier, and the web typecheck. Database integration, migration, end-to-end, evaluator Wasm, and web production build checks run as separate required CI jobs.

The service-free, PostgreSQL, and end-to-end jobs collect coverage in parallel
while running their existing test tiers. A final job merges the three reports,
requires at least 80% aggregate line coverage, and uploads that report under the
`rust-all` Codecov flag. Project coverage and the repository badge therefore
reflect code reached through unit, integration, and distributed runtime tests.
Only PostgreSQL test fixture sources are excluded; production query and table
modules remain in the coverage denominator.

SQLx creates each isolated PostgreSQL test database with `CREATE DATABASE`,
which clones PostgreSQL's built-in `template1` database. The PostgreSQL coverage
job migrates `template1` once, so each test validates the recorded migrations
instead of rebuilding every partition and index. The separate migration job
still applies the complete migration set to an empty database.

Accumulate the same reports locally after starting the services and setting the
environment variables required by each tier:

```bash
cargo llvm-cov clean --workspace
cargo llvm-cov --no-report --workspace --locked --lib --bins
cargo llvm-cov --no-report -p vigilo --locked --bin vigilo -- --ignored --nocapture --test-threads=4
cargo llvm-cov --no-report -p vigilo --locked --test multi_database_routing -- --nocapture
VIGILO_E2E_MULTI_DATABASE=1 cargo llvm-cov --no-report -p vigilo --locked --test multi_database_e2e -- --nocapture
cargo llvm-cov report --fail-under-lines 80 --ignore-filename-regex '[/\\]postgres_tests(\.rs|[/\\])' --lcov --output-path lcov.info
```

The integration commands require the database, messaging, evaluator Wasm, and
test agent dependencies described above. Configure the repository secret
`CODECOV_TOKEN` for the aggregate CI upload.

## Project Status

Agent Vigilo is an active systems project focused on reliable AI evaluation, LLM evaluation workflows, agent testing, and deployment gates. The implementation favors explicit contracts, durable state transitions, and operational diagrams over black-box orchestration.

## License

[MIT](LICENSE)
