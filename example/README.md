# Example Project

This example demonstrates a file-based run profile and dataset that target a local llama.cpp-backed agent from the dev Docker Compose stack.

## Files

- `profile.yaml`: Qwen sentiment evaluation profile using `agent.http`
- `dataset.yaml`: minimal sentiment classification dataset

The profile points at `http://agent_vigilo_agent:8080/v1/chat/completions`, the llama.cpp-backed agent service in `infra/dev/docker-compose.yml`. Workers send an OpenAI-compatible chat completions request and map the agent response into the evaluator `actual` envelope.

## Model

Place the GGUF model at:

```text
models/qwen2.5-0.5b-instruct-q4_k_m.gguf
```

The agent service mounts `./models` into the container as `/models`.

## Evaluator

The profile uses the bundled evaluator `vigilo/sentiment-basic-en:0.1.0`. Build and publish it before creating runs:

```bash
cargo build --manifest-path evaluators/sentiment-basic-en/Cargo.toml --target wasm32-wasip2 --release
DATABASE_URL='postgres://postgres:password@localhost:5432/agent_vigilo' cargo run -p vigilo -- evaluators publish evaluators/sentiment-basic-en --release
```

## Run

```bash
docker compose -f infra/dev/docker-compose.yml up -d
DATABASE_URL='postgres://postgres:password@localhost:5432/agent_vigilo' cargo run -p vigilo -- run test --profile-file example/profile.yaml --dataset-file example/dataset.yaml
```

Notes:

- Inline options (`--profile` / `--dataset`) are supported for quick experiments.
- File options are recommended for large, versioned payloads.
- The profile URL uses the Compose service name because workers run inside the dev Compose network.
