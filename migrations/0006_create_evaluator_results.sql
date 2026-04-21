CREATE TABLE evaluator_results (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    execution_id UUID NOT NULL REFERENCES executions(id) ON DELETE CASCADE,
    attempt_id UUID NOT NULL REFERENCES execution_attempts(id) ON DELETE CASCADE,

    evaluator_id TEXT NOT NULL,
    evaluator_version TEXT NOT NULL,
    evaluator_profile_id TEXT NOT NULL,
    evaluator_profile_version TEXT NOT NULL,

    -- optional interface/runtime versioning for WASM/WIT compatibility
    evaluator_interface_version TEXT,
    evaluator_runtime_version TEXT,

    dimension TEXT NOT NULL,
    status evaluation_status NOT NULL,
    blocking BOOLEAN NOT NULL DEFAULT false,

    score_kind TEXT NOT NULL,
    raw_score DOUBLE PRECISION,
    raw_score_min DOUBLE PRECISION,
    raw_score_max DOUBLE PRECISION,
    normalized_score DOUBLE PRECISION,
    weight DOUBLE PRECISION NOT NULL DEFAULT 1.0 CHECK (weight >= 0),

    severity severity NOT NULL DEFAULT 'none',
    failure_category TEXT,
    reason TEXT,

    evidence JSONB NOT NULL DEFAULT '{}'::jsonb,
    raw_evaluator_output JSONB NOT NULL DEFAULT '{}'::jsonb,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT chk_normalized_score_range
       CHECK (normalized_score IS NULL OR (normalized_score >= 0.0 AND normalized_score <= 1.0)),

    CONSTRAINT uq_attempt_evaluator UNIQUE (attempt_id, evaluator_id)
);

CREATE INDEX idx_evaluator_results_run_id ON evaluator_results(run_id);
CREATE INDEX idx_evaluator_results_execution_id ON evaluator_results(execution_id);
CREATE INDEX idx_evaluator_results_attempt_id ON evaluator_results(attempt_id);
CREATE INDEX idx_evaluator_results_dimension ON evaluator_results(dimension);
CREATE INDEX idx_evaluator_results_status ON evaluator_results(status);
CREATE INDEX idx_evaluator_results_failure_category ON evaluator_results(failure_category);
CREATE INDEX idx_evaluator_results_evaluator_id ON evaluator_results(evaluator_id);
