//! Narrow table access helpers.
//!
//! Table modules map between database rows and model structs. They avoid
//! cross-table orchestration so workflows can compose them or use dedicated
//! transactional SQL where correctness depends on several tables changing
//! together.

pub(crate) mod evaluator_results;
pub(crate) mod evaluators;
pub(crate) mod execution_aggregates;
pub(crate) mod execution_attempts;
pub(crate) mod executions;
pub(crate) mod outbox_events;
pub(crate) mod runs;
