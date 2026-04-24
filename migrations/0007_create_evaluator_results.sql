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

COMMENT ON TABLE evaluator_results IS
    'Append-only records representing the outcome of individual evaluators applied to execution attempts. These rows form the canonical evidence used for aggregation, scoring, and policy decisions.';

COMMENT ON COLUMN evaluator_results.id IS
    'Unique identifier for the evaluator result record.';

COMMENT ON COLUMN evaluator_results.run_id IS
    'Reference to the run this result belongs to. Included for query efficiency and aggregation.';

COMMENT ON COLUMN evaluator_results.execution_id IS
    'Reference to the execution this evaluator result is associated with.';

COMMENT ON COLUMN evaluator_results.attempt_id IS
    'Reference to the specific attempt that produced this evaluator result. Only results from the authoritative (non-stale) attempt should be used for aggregation.';

COMMENT ON COLUMN evaluator_results.evaluator_id IS
    'Identifier of the evaluator that produced this result.';

COMMENT ON COLUMN evaluator_results.evaluator_version IS
    'Version of the evaluator used to produce this result, ensuring reproducibility.';

COMMENT ON COLUMN evaluator_results.evaluator_profile_id IS
    'Identifier of the evaluation profile that included this evaluator.';

COMMENT ON COLUMN evaluator_results.evaluator_profile_version IS
    'Version of the evaluation profile used for this evaluation.';

COMMENT ON COLUMN evaluator_results.evaluator_interface_version IS
    'Version of the evaluator interface contract implemented by this artifact, used for runtime compatibility checks.';

COMMENT ON COLUMN evaluator_results.evaluator_runtime_version IS
    'Version of the runtime or execution environment used to run the evaluator, if applicable.';

COMMENT ON COLUMN evaluator_results.dimension IS
    'Logical dimension this evaluator contributes to (e.g., correctness, safety, quality). Used for aggregation and scoring.';

COMMENT ON COLUMN evaluator_results.status IS
    'Result of the evaluator for this execution. Used as input to aggregation and policy decisions.';

COMMENT ON COLUMN evaluator_results.blocking IS
    'Indicates whether a failure from this evaluator should be treated as blocking (i.e., immediately causing execution or run failure regardless of score).';

COMMENT ON COLUMN evaluator_results.score_kind IS
    'Type of scoring produced by the evaluator (e.g., binary, scalar, categorical). Used to interpret raw and normalized scores.';

COMMENT ON COLUMN evaluator_results.raw_score IS
    'Raw score produced by the evaluator before normalization. Interpretation depends on score_kind.';

COMMENT ON COLUMN evaluator_results.raw_score_min IS
    'Minimum possible value of the raw score, if applicable. Used for normalization.';

COMMENT ON COLUMN evaluator_results.raw_score_max IS
    'Maximum possible value of the raw score, if applicable. Used for normalization.';

COMMENT ON COLUMN evaluator_results.normalized_score IS
    'Score normalized to a standard range (typically 0.0 to 1.0) for aggregation across heterogeneous evaluators.';

COMMENT ON COLUMN evaluator_results.weight IS
    'Weight assigned to this evaluator result during aggregation within its dimension.';

COMMENT ON COLUMN evaluator_results.severity IS
    'Severity level assigned by the evaluator to indicate the impact of a detected issue. Used in aggregation and policy decisions alongside evaluation_status.';

COMMENT ON COLUMN evaluator_results.failure_category IS
    'Optional categorical label describing the type of failure (e.g., hallucination, format_error, unsafe_content). Used for debugging and analysis.';

COMMENT ON COLUMN evaluator_results.reason IS
    'Human-readable explanation of the evaluator result, especially in failure cases.';

COMMENT ON COLUMN evaluator_results.evidence IS
    'Structured evidence supporting the evaluator decision, such as extracted spans, comparison data, or intermediate reasoning artifacts.';

COMMENT ON COLUMN evaluator_results.raw_evaluator_output IS
    'Raw output produced by the evaluator before normalization or interpretation. Useful for debugging and auditability.';

COMMENT ON COLUMN evaluator_results.created_at IS
    'Timestamp when the evaluator result was recorded.';
