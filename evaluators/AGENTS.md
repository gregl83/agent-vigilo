# Creating Evaluators Guidance

This file applies to evaluator crates under `evaluators/`.

## Scope

- Use this file when creating or editing evaluator crates.
- For core project/runtime changes outside evaluator crates, follow `AGENTS.md`.

## Single-Evaluator Crate Standard

- One crate should implement one evaluator component.
- Keep one primary evaluator entrypoint (`evaluate`) in crate code.
- Include `Vigilo.toml` and an `example-input.json` in the crate root.
- Keep evaluator logic self-contained and deterministic where possible.

## Contract Alignment

- Implement the current contract from `wit/evaluator.wit`.
- Read canonical evaluator `input` fields and return canonical `output`.
- Do not introduce alternate envelope names for evaluator entrypoints.

## Build and Test Expectations

- Build target: `wasm32-wasip2`.
- Validate with `vigilo evaluators test` using `--input` or `--input-file`.
- If contract shape changes, bump evaluator version before republishing.

## Reference

- Example crate: `evaluators/sentiment-basic-en`
- Guide: `web/docs/guides/creating-evaluators.mdx`
- Template details: `evaluators/sentiment-basic-en/README.md`
