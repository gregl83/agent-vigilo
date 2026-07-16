# Dev Infrastructure

The single-database Compose file starts the default development stack. The
sharded overlay adds a second PostgreSQL placement and registers it in the
control database for local scale-out testing.

Use the matching env file and Compose file together:

| Topology | Env file | Compose files |
| --- | --- | --- |
| Single database | `infra/dev/.env.single` | `infra/dev/docker-compose.single.yml` |
| Sharded database | `infra/dev/.env.sharded` | `infra/dev/docker-compose.single.yml` + `infra/dev/docker-compose.sharded.yml` |

There is no default `.env` file in this directory. Pass `--env-file`
explicitly so the active topology is visible in the command.

## Single Database

```bash
docker compose --env-file infra/dev/.env.single \
  -f infra/dev/docker-compose.single.yml \
  up -d postgres rabbitmq
```

Run host-side commands with the host URLs from `infra/dev/.env.single`:

```bash
export DATABASE_URL=postgresql://postgres:password@localhost:5432/agent_vigilo
export MESSAGING_URL=amqp://guest:guest@localhost:5672
```

## Sharded

```bash
docker compose --env-file infra/dev/.env.sharded \
  -f infra/dev/docker-compose.single.yml \
  -f infra/dev/docker-compose.sharded.yml \
  up -d postgres postgres-shard-001 rabbitmq
```

Host-side tests and CLI commands use the published ports:

```bash
export DATABASE_URL=postgresql://postgres:password@localhost:5432/agent_vigilo
export VIGILO_TEST_SHARD_001_DATABASE_URL=postgresql://postgres:password@localhost:5433/agent_vigilo
export VIGILO_SHARD_001_DATABASE_URL=postgresql://postgres:password@localhost:5433/agent_vigilo
export MESSAGING_URL=amqp://guest:guest@localhost:5672
```

Run the CI-equivalent routing test:

```bash
cargo test -p vigilo --locked --test multi_database_routing -- --nocapture
```

For a full sharded runtime stack, start all services with the overlay:

```bash
docker compose --env-file infra/dev/.env.sharded \
  -f infra/dev/docker-compose.single.yml \
  -f infra/dev/docker-compose.sharded.yml \
  up -d
```

The overlay migrates `postgres-shard-001` and upserts the `shard_001`
placement row in the control database. Inside containers, database URLs use
Compose service names such as `postgres` and `postgres-shard-001`; host-side
tests use `localhost` and the published ports.

## Reset

Use the same file set for shutdown that you used for startup. Add `-v` when you
need fresh databases.

```bash
docker compose -f infra/dev/docker-compose.single.yml \
  down -v
```

```bash
docker compose -f infra/dev/docker-compose.single.yml \
  -f infra/dev/docker-compose.sharded.yml \
  down -v
```
