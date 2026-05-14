# Example Project

This example demonstrates the `vigilo run test` input format using file-based profile and dataset payloads.

## Files

- `profile.yaml`: release-oriented mixed-task evaluation profile
- `dataset.yaml`: minimal multi-case dataset sample

The profile includes an `agent.http` block. Workers POST a JSON payload with `run_id`, `execution_id`, `attempt_id`, `agent`, `input`, and `case` to that endpoint, then map the response into the evaluator `actual` envelope.

## Run

```bash
DATABASE_URL='postgres://postgres:password@localhost:5432/agent_vigilo' ./target/debug/vigilo.exe run test --profile-file example/profile.yaml --dataset-file example/dataset.yaml
```

Notes:

- Inline options (`--profile` / `--dataset`) are supported for quick experiments.
- File options are recommended for large, versioned payloads.
- Point `agent.http.url` at a reachable local agent service before running workers.
