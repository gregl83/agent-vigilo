# Agent Guidance

This repository supports AI-assisted development, but generated changes must follow project contracts and boundaries.

## Scope

- Use this file for project-level changes (runtime, CLI, contracts, migrations, docs).
- For evaluator crate generation under `evaluators/`, also follow `evaluators/AGENTS.md`.

## Repository Rules

- Keep evaluator interface vocabulary as `input` and `output`.
- Treat `wit/evaluator/v1.0.0/evaluator.wit` as the sole pre-release source of
  truth for the evaluator ABI. Freeze versioned contracts when they are released.
- Keep each supported ABI self-contained under `vigilo/src/evaluator_abi/`: WIT
  bindings, identity, validation, input/output mapping, execution, and fixture.
- Register adapters explicitly in `vigilo/src/evaluator_abi.rs`. Adding an
  ABI must not require worker, aggregation, database, or profile-policy changes.
- A new ABI requires a frozen versioned WIT file, adapter module, real Wasm
  fixture marked with `package.metadata.vigilo.abi-fixture`, registry entry,
  compatibility-matrix coverage, and documentation.
- Retain old adapters while published artifacts or reproducible runs depend on
  them. Unknown identities and altered contract hashes must fail closed.
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

## Pre-PR Checks

- Keep the test-tier names, scopes, and commands aligned with
  [README.md](README.md#test-tiers).
- Reserve `#[ignore]` in the `vigilo` binary test target for the PostgreSQL-backed
  database integration tier. Other opt-in service tests belong in an explicitly
  named integration target and CI tier.
- Before opening or updating a PR that touches Rust code, CI, migrations, evaluator crates, or workflow files, run:
  - `cargo +nightly fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked -- -D warnings`
  - `cargo test --workspace --locked --lib --bins`
- If database workflows, SQL, locking, leases, or sharding behavior changed, run
  the database integration tier against PostgreSQL:
  - `cargo test -p vigilo --locked --bin vigilo -- --ignored --nocapture --test-threads=4`
- If migrations changed, run the setup/migration path against a fresh local PostgreSQL database.
- If distributed routing or runtime behavior changed, run the end-to-end tier
  when its two PostgreSQL servers, RabbitMQ, evaluator Wasm, and test HTTP agent
  are available.
- If web docs changed, run `npm --prefix web run typecheck` and `npm --prefix web run build`.
- If local tooling is unavailable, say so explicitly and treat GitHub Actions as the first verification pass.
- Optional repository hooks can be installed with `chmod +x scripts/hooks/pre-commit scripts/hooks/pre-push` and `git config core.hooksPath scripts/hooks`.
- The pre-commit hook is intentionally light and runs formatting only. The pre-push hook runs clippy, tests, and web typecheck.

## Change Hygiene

- Make focused commits by concern (contracts, runtime, docs, examples).
- Do not rename public CLI flags/terms unless explicitly requested.
- When WIT shapes change, update host mappings, evaluator examples, and docs together.
