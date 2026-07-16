# Dev Infrastructure

This directory defines two local Docker Compose topologies:

| Topology | Env file | Compose files |
| --- | --- | --- |
| Single database | `infra/dev/.env.single` | `infra/dev/docker-compose.single.yml` |
| Sharded database | `infra/dev/.env.sharded` | `infra/dev/docker-compose.single.yml` + `infra/dev/docker-compose.sharded.yml` |

There is no default `.env` file in this directory. Always pass `--env-file`
so the active topology is explicit.

Compose is idempotent for the same project and file set. Running the same
`docker compose up` command again reuses or recreates the same services; it
does not create a second copy of `postgres`, `coordinator`, or `worker`.

## Service Groups

- Infrastructure: `postgres`, `postgres-shard-001`, `rabbitmq`
- Bootstrap: `setup`, `setup-shard-001`, `configure-shard-001`
- Runtime: `agent`, `coordinator`, `worker`

Bootstrap services are one-shot jobs. Runtime services depend on bootstrap, so
starting `coordinator` or `worker` may also start setup/config services.
In the sharded topology, shard setup waits for primary setup so the shared
Cargo cache is not written by two setup jobs at once.

## Agent Model

The `agent` service runs llama.cpp and expects this host file:

```text
models/qwen2.5-0.5b-instruct-q4_k_m.gguf
```

Set `VIGILO_AGENT_MODEL` in the selected env file when using a different GGUF
filename. Start database-only or bootstrap-only commands when you do not need
the local agent container.

## Single Database

Start only Postgres and RabbitMQ:

```bash
docker compose --env-file infra/dev/.env.single \
  -f infra/dev/docker-compose.single.yml \
  up -d postgres rabbitmq
```

Run setup:

```bash
docker compose --env-file infra/dev/.env.single \
  -f infra/dev/docker-compose.single.yml \
  up setup
```

Start the full single-database runtime:

```bash
docker compose --env-file infra/dev/.env.single \
  -f infra/dev/docker-compose.single.yml \
  up -d
```

Host-side commands use published ports:

```bash
export DATABASE_URL=postgresql://postgres:password@localhost:5432/agent_vigilo
export MESSAGING_URL=amqp://guest:guest@localhost:5672
```

## Sharded Database

Start only Postgres primary, Postgres shard, and RabbitMQ:

```bash
docker compose --env-file infra/dev/.env.sharded \
  -f infra/dev/docker-compose.single.yml \
  -f infra/dev/docker-compose.sharded.yml \
  up -d postgres postgres-shard-001 rabbitmq
```

Bootstrap both databases and register `shard_001` in the control DB:

```bash
docker compose --env-file infra/dev/.env.sharded \
  -f infra/dev/docker-compose.single.yml \
  -f infra/dev/docker-compose.sharded.yml \
  up setup setup-shard-001 configure-shard-001
```

Start the full sharded runtime:

```bash
docker compose --env-file infra/dev/.env.sharded \
  -f infra/dev/docker-compose.single.yml \
  -f infra/dev/docker-compose.sharded.yml \
  up -d
```

Host-side tests and CLI commands use published ports:

```bash
export DATABASE_URL=postgresql://postgres:password@localhost:5432/agent_vigilo
export VIGILO_TEST_SHARD_001_DATABASE_URL=postgresql://postgres:password@localhost:5433/agent_vigilo
export VIGILO_SHARD_001_DATABASE_URL=postgresql://postgres:password@localhost:5433/agent_vigilo
export MESSAGING_URL=amqp://guest:guest@localhost:5672
```

Run integration tests against infrastructure and bootstrap services only. Stop
runtime services first so `coordinator` and `worker` do not mutate test runs:

```bash
docker compose --env-file infra/dev/.env.sharded \
  -f infra/dev/docker-compose.single.yml \
  -f infra/dev/docker-compose.sharded.yml \
  stop agent coordinator worker
```

Run the CI-equivalent routing test after exporting the host-side variables:

```bash
cargo test -p vigilo --locked --test multi_database_routing -- --nocapture
```

Run the end-to-end multi-database harness when RabbitMQ is also running and the
bundled evaluator WASM has been built:

```bash
cargo build -p sentiment-basic-en --target wasm32-wasip2 --release --locked

VIGILO_E2E_MULTI_DATABASE=1 \
cargo test -p vigilo --locked --test multi_database_e2e -- --nocapture
```

The E2E harness uses a per-run `VIGILO_MQ_NAMESPACE`, so it does not consume
messages left by a previous local runtime stack.

## Runtime Dependencies

This command starts the runtime services:

```bash
docker compose --env-file infra/dev/.env.sharded \
  -f infra/dev/docker-compose.single.yml \
  -f infra/dev/docker-compose.sharded.yml \
  up -d agent coordinator worker
```

Compose may also start `setup`, `setup-shard-001`, `configure-shard-001`,
`postgres`, `postgres-shard-001`, and `rabbitmq` because they are dependencies.
That is expected. `postgres` and `postgres-shard-001` are shared database
services, not separate databases for the coordinator and worker.

Inside containers, database URLs use Compose DNS names:

```text
postgres:5432
postgres-shard-001:5432
rabbitmq:5672
```

Host-side tests and CLI commands use `localhost` and the published ports.

## Reset

Use the same file set for shutdown that you used for startup.

Single database:

```bash
docker compose --env-file infra/dev/.env.single \
  -f infra/dev/docker-compose.single.yml \
  down --remove-orphans
```

Sharded database:

```bash
docker compose --env-file infra/dev/.env.sharded \
  -f infra/dev/docker-compose.single.yml \
  -f infra/dev/docker-compose.sharded.yml \
  down --remove-orphans
```

Add `-v` when you need fresh databases:

```bash
docker compose --env-file infra/dev/.env.sharded \
  -f infra/dev/docker-compose.single.yml \
  -f infra/dev/docker-compose.sharded.yml \
  down -v --remove-orphans
```

If a setup job failed while Cargo was downloading crates, clear only the Cargo
registry cache and rerun setup:

```bash
docker compose --env-file infra/dev/.env.sharded \
  -f infra/dev/docker-compose.single.yml \
  -f infra/dev/docker-compose.sharded.yml \
  down --remove-orphans

docker volume rm agent_vigilo_dev_agent_vigilo_cargo_registry
```
