CREATE TABLE execution_attempts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    execution_id UUID NOT NULL REFERENCES executions(id) ON DELETE CASCADE,
    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,

    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
    status attempt_status NOT NULL DEFAULT 'pending',

    worker_id TEXT,
    worker_host TEXT,
    queue_message_id TEXT,

    -- lease/heartbeat for distributed recovery
    leased_until TIMESTAMPTZ,
    heartbeat_at TIMESTAMPTZ,

    -- raw artifacts should generally live outside the DB;
    -- keep references here
    request_artifact_uri TEXT,
    response_artifact_uri TEXT,

    -- timing
    agent_latency_ms BIGINT CHECK (agent_latency_ms IS NULL OR agent_latency_ms >= 0),
    evaluator_latency_ms BIGINT CHECK (evaluator_latency_ms IS NULL OR evaluator_latency_ms >= 0),
    total_latency_ms BIGINT CHECK (total_latency_ms IS NULL OR total_latency_ms >= 0),

    -- optional usage/cost summary
    token_usage JSONB NOT NULL DEFAULT '{}'::jsonb,

    -- compact attempt-level summary
    outcome_summary JSONB NOT NULL DEFAULT '{}'::jsonb,

    error_message TEXT,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT uq_execution_attempt_no UNIQUE (execution_id, attempt_no)
);

CREATE INDEX idx_execution_attempts_run_id ON execution_attempts(run_id);
CREATE INDEX idx_execution_attempts_execution_id ON execution_attempts(execution_id);
CREATE INDEX idx_execution_attempts_status ON execution_attempts(status);
CREATE INDEX idx_execution_attempts_lease ON execution_attempts(leased_until)
    WHERE status = 'running';
CREATE INDEX idx_execution_attempts_run_status ON execution_attempts(run_id, status);

COMMENT ON COLUMN execution_attempts.status IS
'State of this specific attempt. Multiple attempts may exist for a single execution; only the most recent non-stale attempt is authoritative.';
