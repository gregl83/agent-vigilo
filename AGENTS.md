# Agent Guidance

This repository supports AI-assisted development, but generated changes must follow project contracts and boundaries.

## Scope

- Use this file for project-level changes (runtime, CLI, contracts, migrations, docs).
- For evaluator crate generation under `evaluators/`, also follow `evaluators/AGENTS.md`.

## Repository Rules

- Keep evaluator interface vocabulary as `input` and `output`.
- Treat `wit/evaluator.wit` as the source of truth for evaluator ABI.
- Keep evaluator execution contracts in `vigilo/src/contracts/`, not persistence models.
- Preserve strict evaluator identifier format: `<namespace>/<name>:<version>`.
- Avoid broad refactors outside the requested scope.

## Documentation and Examples

- Prefer linking to existing guides rather than duplicating long instructions.
- Use `evaluators/sentiment-basic-en/README.md` as the primary single-evaluator reference.
- Keep examples small, runnable, and versioned.

## Rustfmt and Toolchain

- The repository default toolchain is `stable` (see `rust-toolchain.toml`).
- `rustfmt.toml` enables unstable rustfmt options (`unstable_features`, import grouping/granularity/layout).
- Use nightly rustfmt when formatting: `cargo +nightly fmt --all`.
- Keep build/test commands on stable unless a task explicitly requires otherwise.
- Do not remove or downgrade rustfmt settings to avoid nightly usage unless explicitly requested.

## Change Hygiene

- Make focused commits by concern (contracts, runtime, docs, examples).
- Do not rename public CLI flags/terms unless explicitly requested.
- When WIT shapes change, update host mappings, evaluator examples, and docs together.
