# Sentiment Basic EN Evaluator

Basic English-only lexicon sentiment evaluator for Agent Vigilo.

Template reference for cloning into a new single-evaluator crate: `evaluators/sentiment-basic-en/README.md`

General guide: `web/docs/guides/creating-evaluators.mdx`

Shared evaluator guidance: `evaluators/AGENTS.md`

## Input

The evaluator consumes a WIT `input` envelope. This evaluator reads text from:

- `input.actual.text` (preferred)
- `input.test_case.input_json.user_message` (fallback)

Text values can be plain text or a JSON object containing a `text` field.

## Limitations

- This evaluator is intentionally basic and intended for example/template use.
- Language handling is advisory and tuned for English terms only.
- It uses a small fixed lexicon and does not perform advanced linguistic analysis.

## Output

The evaluator returns a structured `output` object:

- `output.evaluator`: identity metadata (`namespace`, `name`, `version`)
- `output.results`: findings array (this evaluator emits one finding)
- `output.metadata_json`: JSON string with approach, language scope, and maturity hints

The finding includes:

- `dimension`: `quality`
- `status`: `passed` for neutral/positive, `failed` for negative
- `score`: normalized sentiment score (`1.0` positive, `0.5` neutral, `0.0` negative)
- `severity`: `medium` for negative, otherwise `none`
- `evidence_json`: JSON string with `label`, `score`, matches, and normalized text
- `tags`: includes `basic` and `english-only` to make scope explicit

## Build

```bash
cargo build --manifest-path evaluators/sentiment-basic-en/Cargo.toml --target wasm32-wasip2 --release
```

## Test

```bash
vigilo evaluators test 'vigilo/sentiment-basic-en:0.1.0' --input-file evaluators/sentiment-basic-en/example-input.json
```

