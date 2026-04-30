# Sentiment Evaluator

Lexicon-based sentiment evaluator for Agent Vigilo.

## Input

`context.db` accepts either:

- Plain text (example: `"great support and fast response"`)
- JSON object with a `text` field (example: `{"text":"great support"}`)

## Output

The evaluator returns a structured `output` object:

- `output.evaluator`: identity metadata (`namespace`, `name`, `version`)
- `output.results`: findings array (this evaluator emits one finding)
- `output.metadata_json`: JSON string for evaluator-level metadata

The finding includes:

- `dimension`: `quality`
- `status`: `passed` for neutral/positive, `failed` for negative
- `score`: normalized sentiment score (`1.0` positive, `0.5` neutral, `0.0` negative)
- `severity`: `medium` for negative, otherwise `none`
- `evidence_json`: JSON string with `label`, `score`, matches, and normalized text

## Build

```bash
cargo build --manifest-path evaluators/sentiment/Cargo.toml --target wasm32-wasip2 --release
```
