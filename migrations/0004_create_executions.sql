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
