//! Multi-table database workflows.
//!
//! Workflow modules own operations where table state must move together:
//! creating runs, leasing work, processing executions, dispatching events, and
//! finalizing aggregate run status. These functions are the place for
//! concurrency guards such as leases, attempt authority checks, and idempotent
//! outbox writes.
//!
//! Reading guide:
//! - `run_create` writes immutable dataset/run/chunk seed state, but does not
//!   make work visible to workers.
//! - `run_dispatch` is coordinator-owned visibility and expired chunk lease
//!   recovery.
//! - `chunk_processing` is worker-owned chunk lease claim/release/completion.
//! - `execution_processing` allocates case attempts, persists evaluator
//!   evidence, schedules retries, and applies current-attempt guarded terminal
//!   transitions.
//! - `run_finalize` rolls terminal chunks/executions into one completed run.
//! - `run_cancel` closes open work and emits an idempotent cancellation event.
//! - `run_profile_validation` verifies profile/dataset/evaluator compatibility
//!   before durable run work is created.

pub(crate) mod chunk_processing;
pub(crate) mod execution_processing;
pub(crate) mod run_cancel;
pub(crate) mod run_create;
pub(crate) mod run_dispatch;
pub(crate) mod run_finalize;
pub(crate) mod run_profile_validation;
