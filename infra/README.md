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

## Performance Compose

`infra/performance/compose.yml` defines the private PostgreSQL and RabbitMQ
topology used by `cargo perf`. The harness creates a unique Compose project,
loopback ports, database namespace, broker scope, labelled network, and labelled
volumes for each campaign. Operators do not start this topology directly;
service-backed performance workloads provision and remove only their recorded
resources.

## AWS Performance Host

`infra/performance/aws/template.yaml` provisions the ephemeral EC2 host for the
canonical performance environment. Its fixed instance, AMI, Availability Zone,
and EBS settings implement the contract in
`performance/environments/aws-m6i-2xlarge-al2023-v1.toml`; deployment and
teardown commands are documented in `infra/performance/aws/README.md`.
