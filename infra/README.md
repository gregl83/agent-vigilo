# Infrastructure

System components for running Agent Vigilo.

## Dev Compose

`infra/dev/docker-compose.yml` starts Postgres, RabbitMQ, Vigilo services, and a local llama.cpp-backed agent service for the example profile.

The agent service expects this model file to exist before startup:

```text
models/qwen2.5-0.5b-instruct-q4_k_m.gguf
```
