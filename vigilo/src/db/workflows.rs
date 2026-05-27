//! Multi-table database workflows.
//!
//! Workflow modules own operations where table state must move together:
//! creating runs, leasing work, processing executions, dispatching events, and
//! finalizing aggregate run status. These functions are the place for
//! concurrency guards such as leases, attempt authority checks, and idempotent
//! outbox writes.

pub(crate) mod chunk_processing;
pub(crate) mod execution_processing;
pub(crate) mod run_cancel;
pub(crate) mod run_create;
pub(crate) mod run_dispatch;
pub(crate) mod run_finalize;
pub(crate) mod run_profile_validation;
