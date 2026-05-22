[![Build](https://github.com/gregl83/agent-vigilo/actions/workflows/build.yml/badge.svg)](https://github.com/gregl83/agent-vigilo/actions/workflows/build.yml)
[![Coverage Status](https://codecov.io/gh/gregl83/agent-vigilo/graph/badge.svg?token=CL93O7DW9C)](https://codecov.io/gh/gregl83/agent-vigilo)
[![Crates.io](https://img.shields.io/crates/v/agent-vigilo.svg)](https://crates.io/crates/agent-vigilo)
[![Documentation](https://img.shields.io/badge/docs-agentvigilo.com-blue.svg)](https://agentvigilo.com/docs/guides/getting-started)
[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

# Agent Vigilo

**Distributed AI evaluation infrastructure and deployment gating for generative AI systems.**

Agent Vigilo turns LLM and agent evaluation into a production runtime: versioned WASM evaluators, durable evaluation runs, worker/coordinator execution, normalized results, and pass/fail gates that can sit in CI or release workflows.

It is built for the parts of AI evaluation that become hard at scale: idempotent distributed work, durable event delivery, evaluator isolation, retry-safe persistence, and auditable results.

## Why It Matters

- **Run evaluations like infrastructure**: PostgreSQL-backed state, RabbitMQ work distribution, Rust workers, and deterministic state guards.
- **Ship versioned evaluators**: publish WASI Preview 2 WebAssembly evaluators with strict WIT contracts.
- **Protect the runtime**: Wasmtime fuel, memory, timeout, log, and concurrency limits isolate evaluator execution.
- **Avoid lost events**: durable outbox ledger plus hot delivery queue, RabbitMQ publisher confirms, and idempotency keys.
- **Gate deployments**: aggregate evaluator results into reproducible pass/fail decisions for agent releases.

## Start Here

- [Getting started](https://agentvigilo.com/docs/guides/getting-started): run your first evaluation.
- [Architecture overview](https://agentvigilo.com/docs/architecture/structure/): containers, components, flows, and state diagrams.
- [Worker runtime](https://agentvigilo.com/docs/architecture/structure/components/worker/): chunk claiming, evaluator execution, and result persistence.
- [Runtime limits](https://agentvigilo.com/docs/configuration/runtime-limits): Wasm evaluator sandbox and worker concurrency controls.
- [Outbox lifecycle](https://agentvigilo.com/docs/architecture/state/outbox-lifecycle/): durable event publication and retry behavior.
- [Publishing evaluators](https://agentvigilo.com/docs/guides/publishing): build and publish versioned WASM evaluators.

## Core Stack

Rust, Tokio, PostgreSQL, SQLx, RabbitMQ, Wasmtime, WASI Preview 2, WIT, Docusaurus.

## Project Status

Agent Vigilo is an active systems project focused on reliable AI evaluation, LLM evaluation workflows, agent testing, and deployment gates. The implementation favors explicit contracts, durable state transitions, and operational diagrams over black-box orchestration.

## License

[MIT](LICENSE)
