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
- Return one primary measurement or abstention and zero or more diagnostics;
  evaluators must not implement dimension, threshold, weight, or blocking policy.
- Measurements are raw `binary`, `numeric`, or `ordinal` observations. Do not
  normalize values or assign ordinal utilities inside evaluator code.
- Do not introduce alternate envelope names for evaluator entrypoints.

## Build and Test Expectations

- Build target: `wasm32-wasip2`.
- Validate with `vigilo evaluator test` using `--input` or `--input-file`.
- If contract shape changes, bump evaluator version before republishing.

## Rustfmt and Toolchain

- Evaluator crates follow the repository toolchain baseline: `stable` by default.
- Formatting still requires nightly rustfmt because root `rustfmt.toml` uses unstable options.
- Format evaluator changes with nightly rustfmt (for example from repo root): `cargo +nightly fmt --all`.
- Keep evaluator build/test flows on stable unless a task explicitly requires otherwise.
- Do not change rustfmt settings just to force stable-only formatting unless explicitly requested.

## Reference

- Example crate: `evaluators/sentiment-basic-en`
- Guide: `web/docs/guides/creating-evaluators.mdx`
- Normalization examples: `web/docs/configuration/measurement-normalization.mdx`
- Template details: `evaluators/sentiment-basic-en/README.md`
