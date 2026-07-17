# Vigilo

Command Line Interface application for operating Agent Vigilo.

## Package Contents

Quick-search overview of the main modules under `vigilo/src`.

| Module | Summary |
| --- | --- |
| `main` | CLI binary entry point: argument parsing, logging setup, context creation, and exit codes. |
| `cli` | Clap application definition, global flags, and command dispatch. |
| `cli::commands` | Top-level command router for setup, evaluator, run, coordinator, and worker commands. |
| `cli::commands::setup` | Applies database migrations and optionally publishes built-in evaluators. |
| `cli::commands::evaluators` | Publish, search, show, test, and state-management commands for evaluator registry entries. |
| `cli::commands::run` | Run creation, validation, status, watch, cancellation, result summary, and export commands. |
| `cli::commands::coordinator` | Coordinator process modes and orchestration cycle for creation recovery, dispatch, finalization, and outbox publishing. |
| `cli::commands::worker` | Worker process modes and chunk execution flow for agent calls and evaluator processing. |
| `cli::args` | Shared clap value parsers for existing files and directories. |
| `context` | Lazy process-wide service container for database, HTTP, messaging, output, registry, and Wasm runtime. |
| `context::database` | Lifetime PostgreSQL connection configuration and lazy pools with live placement status and role admission. |
| `context::http` | Lazy shared `reqwest` client initialization. |
| `context::messaging` | Lazy RabbitMQ client initialization. |
| `context::output` | Structured stdout writer for JSON and TOON command output. |
| `context::registry` | In-memory cache for compiled evaluator Wasm components. |
| `context::wasm` | Wasmtime setup, evaluator artifact preparation, WIT mapping, and evaluator test execution. |
| `contracts` | Host-side runtime contracts for run inputs, evaluator I/O, evaluator refs, and aggregation. |
| `contracts::run` | Run profile, agent config, aggregation policy, and dataset input payloads. |
| `contracts::evaluator` | Canonical evaluator `input` and `output` payloads plus normalization helpers. |
| `contracts::evaluator_ref` | Parser for fully qualified evaluator ids: `<namespace>/<name>:<version>`. |
| `contracts::aggregation` | Runtime aggregation policy for normalized evaluator findings. |
| `db` | Database access layer split into migrations, table helpers, and multi-table workflows. |
| `db::migrations` | SQL migration runner. |
| `db::tables` | Narrow row-oriented database helpers. |
| `db::workflows` | Transactional and concurrency-aware database operations for runs, chunks, executions, and cancellation. |
| `db::workflows::run_creation` | Recoverable cross-database run creation, idempotent placement seeding, and atomic dispatch activation. |
| `models` | Persistence model structs that mirror database rows and drafts. |
| `agent_client` | Worker-side HTTP client for invoking configured agent endpoints and normalizing responses. |
| `evaluators` | Built-in evaluator discovery, build, bootstrap, and publishing workflows. |
| `manifest` | `Vigilo.toml` evaluator manifest parsing. |
| `mq` | RabbitMQ topology, publishing, worker consumption, retry, and quarantine handling. |
| `outbox` | Durable outbox event publication from database rows to the message broker. |
| `runtime` | Generic service runner for long-running coordinator and worker processes. |
