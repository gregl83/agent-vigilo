CREATE TYPE run_status AS ENUM (
  'pending',
  'running',
  'finalizing',
  'completed',
  'failed',
  'cancelled'
);
COMMENT ON TYPE run_status IS
'Lifecycle state of an evaluation run. Represents orchestration progress from creation through finalization.';
COMMENT ON ENUM run_status VALUE 'pending' IS
'Run has been created but no executions have been dispatched yet.';
COMMENT ON ENUM run_status VALUE 'running' IS
'Executions have been dispatched and are actively being processed by workers.';
COMMENT ON ENUM run_status VALUE 'finalizing' IS
'All executions are terminal; run-level aggregation, summary computation, and event emission are in progress.';
COMMENT ON ENUM run_status VALUE 'completed' IS
'Run has finished successfully with a final gate decision (pass or fail) and no remaining work.';
COMMENT ON ENUM run_status VALUE 'failed' IS
'Run failed due to a system-level issue (e.g., infrastructure error, unrecoverable failure), not just failing evaluations.';
COMMENT ON ENUM run_status VALUE 'cancelled' IS
'Run was intentionally stopped before completion; some executions may be incomplete or abandoned.';

CREATE TYPE gate_status AS ENUM (
  'unknown',
  'pass',
  'fail'
);
COMMENT ON TYPE gate_status IS
'Final evaluation outcome of a run based on aggregation policy. Separate from run lifecycle (run_status).';
COMMENT ON ENUM gate_status VALUE 'unknown' IS
'Gate result has not yet been determined. Run is still in progress or has not been evaluated.';
COMMENT ON ENUM gate_status VALUE 'pass' IS
'Run met all evaluation criteria and policy thresholds. Safe to proceed with downstream actions such as deployment.';
COMMENT ON ENUM gate_status VALUE 'fail' IS
'Run did not meet evaluation criteria or violated blocking conditions. Should prevent downstream actions such as deployment.';

CREATE TYPE execution_status AS ENUM (
  'pending',
  'running',
  'awaiting_evaluators',
  'retry_scheduled',
  'completed',
  'failed',
  'timed_out',
  'cancelled'
);
COMMENT ON TYPE execution_status IS
'Lifecycle state of a single execution (one dataset case). Tracks progress from scheduling through agent execution, evaluation, and terminal outcome.';
COMMENT ON ENUM execution_status VALUE 'pending' IS
'Execution has been created but not yet claimed or started by a worker.';
COMMENT ON ENUM execution_status VALUE 'running' IS
'Execution is actively being processed by a worker, including agent invocation and evaluator execution.';
COMMENT ON ENUM execution_status VALUE 'awaiting_evaluators' IS
'Agent output has been produced, but evaluator results are not yet complete or fully persisted.';
COMMENT ON ENUM execution_status VALUE 'retry_scheduled' IS
'Execution previously failed but is eligible for retry; a future attempt is expected.';
COMMENT ON ENUM execution_status VALUE 'completed' IS
'Execution finished successfully with all required evaluators completed and an aggregate result produced.';
COMMENT ON ENUM execution_status VALUE 'failed' IS
'Execution reached a terminal failure state after exhausting retries or encountering a non-recoverable error.';
COMMENT ON ENUM execution_status VALUE 'timed_out' IS
'Execution did not complete within allowed time bounds and was marked terminal.';
COMMENT ON ENUM execution_status VALUE 'cancelled' IS
'Execution was intentionally stopped before completion, typically due to run cancellation.';

CREATE TYPE attempt_status AS ENUM (
  'pending',
  'running',
  'completed',
  'failed_agent_call',
  'failed_evaluation',
  'timed_out',
  'cancelled',
  'stale'
);
COMMENT ON TYPE attempt_status IS
'Lifecycle state of a single execution attempt. Represents one worker-owned attempt to process an execution, including leasing, execution, evaluation, and termination conditions.';
COMMENT ON ENUM attempt_status VALUE 'pending' IS
'Attempt has been created but not yet claimed by a worker. No work has started.';
COMMENT ON ENUM attempt_status VALUE 'running' IS
'Attempt is actively being processed by a worker. The worker is expected to hold a valid lease and may be invoking the agent and running evaluators.';
COMMENT ON ENUM attempt_status VALUE 'completed' IS
'Attempt finished successfully with agent execution and evaluation complete. Results should be fully persisted.';
COMMENT ON ENUM attempt_status VALUE 'failed_agent_call' IS
'Attempt failed during agent invocation (e.g., HTTP error, timeout, or upstream failure) before evaluation could complete.';
COMMENT ON ENUM attempt_status VALUE 'failed_evaluation' IS
'Attempt produced agent output but failed during evaluator execution or result persistence.';
COMMENT ON ENUM attempt_status VALUE 'timed_out' IS
'Attempt exceeded allowed execution time or lease duration and was marked as terminal. Worker may have crashed or become unresponsive.';
COMMENT ON ENUM attempt_status VALUE 'cancelled' IS
'Attempt was intentionally terminated before completion, typically due to run cancellation or manual intervention.';
COMMENT ON ENUM attempt_status VALUE 'stale' IS
'Attempt is no longer authoritative due to lease expiration or reassignment. A newer attempt is expected to replace it.';

CREATE TYPE evaluation_status AS ENUM (
  'passed',
  'failed',
  'error',
  'skipped'
);
COMMENT ON TYPE evaluation_status IS
'Outcome of a single evaluator applied to an execution. Represents the evaluator’s judgment, independent of aggregation or overall execution result.';
COMMENT ON ENUM evaluation_status VALUE 'passed' IS
'Evaluator determined the output meets the expected criteria or constraints for this check.';
COMMENT ON ENUM evaluation_status VALUE 'failed' IS
'Evaluator determined the output violates expected criteria, constraints, or correctness requirements.';
COMMENT ON ENUM evaluation_status VALUE 'error' IS
'Evaluator failed to produce a valid result due to an internal error, runtime failure, or invalid input. Does not necessarily indicate the output itself is incorrect.';
COMMENT ON ENUM evaluation_status VALUE 'skipped' IS
'Evaluator was not applied to this execution, typically due to conditional logic, unsupported input, or configuration rules.';

CREATE TYPE severity AS ENUM (
  'none',
  'low',
  'medium',
  'high',
  'critical'
);
COMMENT ON TYPE severity IS
'Indicates the magnitude or impact of an evaluator finding. Used to qualify failures or issues beyond simple pass/fail outcomes.';
COMMENT ON ENUM severity VALUE 'none' IS
'No issue detected or not applicable. Used when the evaluator does not report any concern.';
COMMENT ON ENUM severity VALUE 'low' IS
'Minor issue with limited impact. Typically acceptable depending on policy thresholds.';
COMMENT ON ENUM severity VALUE 'medium' IS
'Moderate issue that may affect correctness, quality, or user experience. Often requires attention or may trigger policy conditions.';
COMMENT ON ENUM severity VALUE 'high' IS
'Serious issue with significant impact. Likely to contribute to execution failure or policy violations.';
COMMENT ON ENUM severity VALUE 'critical' IS
'Severe issue indicating unacceptable behavior (e.g., safety violations, major correctness failures). Typically should block deployment regardless of other scores.';

CREATE TYPE outbox_status AS ENUM (
  'pending',
  'published',
  'failed'
);
COMMENT ON TYPE outbox_status IS
'Represents the delivery state of an outbox event. Used to ensure reliable, idempotent publication of events to external systems.';
COMMENT ON ENUM outbox_status VALUE 'pending' IS
'Event has been created and is ready to be published, but has not yet been processed by the outbox publisher.';
COMMENT ON ENUM outbox_status VALUE 'published' IS
'Event has been successfully delivered to the external system or message broker.';
COMMENT ON ENUM outbox_status VALUE 'failed' IS
'Event publication attempt failed. The event may be retried depending on retry policy and error handling.';
