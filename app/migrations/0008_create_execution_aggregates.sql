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

COMMENT ON TABLE execution_aggregates IS
    'Stores the current authoritative aggregate result for an execution attempt. Derived from append-only evaluator results and used for execution- and run-level summaries, scoring, and gate decisions.';

COMMENT ON COLUMN execution_aggregates.execution_id IS
    'Reference to the execution this aggregate summarizes. One authoritative aggregate exists per execution.';

COMMENT ON COLUMN execution_aggregates.run_id IS
    'Reference to the run this execution aggregate belongs to. Included for query efficiency and run-level aggregation.';

COMMENT ON COLUMN execution_aggregates.attempt_id IS
    'Reference to the authoritative attempt whose evaluator results were used to produce this aggregate.';

COMMENT ON COLUMN execution_aggregates.overall_status IS
    'Final aggregated status for the execution, derived from evaluator results and aggregation policy.';

COMMENT ON COLUMN execution_aggregates.aggregate_score IS
    'Final normalized aggregate score for the execution, typically in the range 0.0 to 1.0.';

COMMENT ON COLUMN execution_aggregates.evaluator_result_count IS
    'Number of evaluator result records used to compute this aggregate.';

COMMENT ON COLUMN execution_aggregates.dimension_scores IS
    'Structured map of per-dimension aggregate scores (e.g., correctness, safety, quality) derived from evaluator results.';

COMMENT ON COLUMN execution_aggregates.blocking_failures IS
    'Structured list of blocking failures that affected the execution outcome, if any.';

COMMENT ON COLUMN execution_aggregates.summary IS
    'Compact execution-level summary including derived metrics, failure counts, and other aggregation outputs used for reporting and debugging.';

COMMENT ON COLUMN execution_aggregates.created_at IS
    'Timestamp when the aggregate record was first created.';

COMMENT ON COLUMN execution_aggregates.updated_at IS
    'Timestamp of the most recent update to the aggregate record.';
