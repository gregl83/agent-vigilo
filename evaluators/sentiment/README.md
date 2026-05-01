# Sentiment Evaluator

Lexicon-based sentiment evaluator for Agent Vigilo.

## Input

The evaluator consumes a WIT `input` envelope. This evaluator reads text from:

- `input.actual.text` (preferred)
- `input.test_case.input_json.user_message` (fallback)

Text values can be plain text or a JSON object containing a `text` field.

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
