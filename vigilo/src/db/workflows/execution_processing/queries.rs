//! PostgreSQL operations for execution processing.

mod allocation;
mod results;
mod transitions;

pub(super) use allocation::allocate_execution_attempts_for_cases;
pub(super) use results::persist_completed_execution_results_batch;
pub(super) use transitions::heartbeat_running_attempts_for_chunk_query;
pub(crate) use transitions::{
    finalize_execution_terminal_transitions,
    summarize_chunk_execution_state,
};
