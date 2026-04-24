CREATE TABLE executions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,

    -- stable identity of the dataset case within a run
    case_id TEXT NOT NULL,

    task_type TEXT NOT NULL,
    tags JSONB NOT NULL DEFAULT '[]'::jsonb,

    -- frozen case payload for reproducibility
    input_payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    expected_output JSONB NOT NULL DEFAULT '{}'::jsonb,
    case_metadata JSONB NOT NULL DEFAULT '{}'::jsonb,

    -- resolved evaluator manifest for this execution
    evaluation_profile_id TEXT NOT NULL,
    evaluation_profile_version TEXT NOT NULL,
    evaluator_manifest JSONB NOT NULL DEFAULT '[]'::jsonb,
    expected_evaluator_count INTEGER NOT NULL DEFAULT 0 CHECK (expected_evaluator_count >= 0),

    -- orchestration state
    status execution_status NOT NULL DEFAULT 'pending',

    current_attempt_no INTEGER NOT NULL DEFAULT 0 CHECK (current_attempt_no >= 0),
    current_attempt_id UUID,

    last_error_message TEXT,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT uq_execution_run_case UNIQUE (run_id, case_id)
);

CREATE INDEX idx_executions_run_id ON executions(run_id);

CREATE INDEX idx_executions_run_status ON executions(run_id, status);

CREATE INDEX idx_executions_current_attempt_id ON executions(current_attempt_id);

COMMENT ON TABLE executions IS
    'Represents a single evaluation of a dataset case against the target system. Each execution is part of a run and may have multiple attempts due to retries or failures.';

COMMENT ON COLUMN executions.id IS
    'Unique identifier for the execution.';

COMMENT ON COLUMN executions.run_id IS
    'Reference to the run this execution belongs to. Determines shared configuration and aggregation context.';

COMMENT ON COLUMN executions.case_id IS
    'Identifier of the dataset case within the run. Unique per run and used to correlate input, expected output, and results.';

COMMENT ON COLUMN executions.task_type IS
    'Logical task category for the execution (e.g., classification, generation, tool-use). Used for routing or conditional evaluation.';

COMMENT ON COLUMN executions.tags IS
    'Optional tags associated with the dataset case for filtering, grouping, or analysis.';

COMMENT ON COLUMN executions.input_payload IS
    'Serialized input provided to the target system for this execution. Represents the dataset case input.';

COMMENT ON COLUMN executions.expected_output IS
    'Optional expected output or reference answer for the dataset case. Used by certain evaluators for correctness checks.';

COMMENT ON COLUMN executions.case_metadata IS
    'Additional metadata associated with the dataset case, such as difficulty, source, or annotations.';

COMMENT ON COLUMN executions.evaluation_profile_id IS
    'Identifier of the evaluation profile applied to this execution.';

COMMENT ON COLUMN executions.evaluation_profile_version IS
    'Version of the evaluation profile used to ensure consistent evaluator configuration.';

COMMENT ON COLUMN executions.evaluator_manifest IS
    'Resolved list of evaluators to be applied to this execution. Stored as a snapshot to ensure reproducibility and to verify completeness.';

COMMENT ON COLUMN executions.expected_evaluator_count IS
    'Number of evaluators expected to run for this execution, derived from the evaluator manifest.';

COMMENT ON COLUMN executions.status IS
    'Current lifecycle state of the execution. Tracks progress from scheduling through processing, evaluation, and terminal outcome. See execution_status enum for details.';

COMMENT ON COLUMN executions.current_attempt_no IS
    'Sequence number of the current attempt for this execution. Increments on each retry.';

COMMENT ON COLUMN executions.current_attempt_id IS
    'Reference to the currently active attempt. Used to determine the authoritative attempt for this execution.';

COMMENT ON COLUMN executions.last_error_message IS
    'Most recent error encountered during execution processing. Useful for debugging failures and retry behavior.';

COMMENT ON COLUMN executions.created_at IS
    'Timestamp when the execution was created.';

COMMENT ON COLUMN executions.started_at IS
    'Timestamp when the execution was first claimed and processing began.';

COMMENT ON COLUMN executions.completed_at IS
    'Timestamp when the execution reached a terminal state.';

COMMENT ON COLUMN executions.updated_at IS
    'Timestamp of the last update to the execution record.';
