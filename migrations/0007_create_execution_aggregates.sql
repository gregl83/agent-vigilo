CREATE TABLE execution_aggregates (
    execution_id UUID PRIMARY KEY REFERENCES executions(id) ON DELETE CASCADE,
    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    attempt_id UUID NOT NULL UNIQUE REFERENCES execution_attempts(id) ON DELETE CASCADE,

    overall_status evaluation_status NOT NULL,
    aggregate_score DOUBLE PRECISION,
    evaluator_result_count INTEGER NOT NULL DEFAULT 0 CHECK (evaluator_result_count >= 0),

    dimension_scores JSONB NOT NULL DEFAULT '{}'::jsonb,
    blocking_failures JSONB NOT NULL DEFAULT '[]'::jsonb,
    summary JSONB NOT NULL DEFAULT '{}'::jsonb,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT chk_execution_aggregate_score_range
      CHECK (aggregate_score IS NULL OR (aggregate_score >= 0.0 AND aggregate_score <= 1.0))
);

CREATE INDEX idx_execution_aggregates_run_id ON execution_aggregates(run_id);
CREATE INDEX idx_execution_aggregates_overall_status ON execution_aggregates(overall_status);
