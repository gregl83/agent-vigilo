# Infrastructure

System components for running Agent Vigilo.

## Dev Compose

`infra/dev/docker-compose.single.yml` starts Postgres, RabbitMQ, Vigilo services, and a local llama.cpp-backed agent service for the example profile. Add `infra/dev/docker-compose.sharded.yml` for a second PostgreSQL shard placement.

The agent service expects this model file to exist before startup:

```text
models/qwen2.5-0.5b-instruct-q4_k_m.gguf
```

Set `VIGILO_AGENT_MODEL` in `infra/dev/.env.single` or
`infra/dev/.env.sharded` when using a different GGUF filename.
