CREATE TABLE run_shard_summaries (
    run_id UUID NOT NULL,
    run_shard SMALLINT NOT NULL CHECK (run_shard >= 0 AND run_shard < 128),
    expected_execution_count INTEGER NOT NULL CHECK (expected_execution_count >= 0),
    terminal_execution_count INTEGER NOT NULL DEFAULT 0 CHECK (terminal_execution_count >= 0),
    passed_execution_count INTEGER NOT NULL DEFAULT 0 CHECK (passed_execution_count >= 0),
    failed_execution_count INTEGER NOT NULL DEFAULT 0 CHECK (failed_execution_count >= 0),
    errored_execution_count INTEGER NOT NULL DEFAULT 0 CHECK (errored_execution_count >= 0),
    missing_aggregate_count INTEGER NOT NULL DEFAULT 0 CHECK (missing_aggregate_count >= 0),
    failed_chunk_count INTEGER NOT NULL DEFAULT 0 CHECK (failed_chunk_count >= 0),
    cancelled_chunk_count INTEGER NOT NULL DEFAULT 0 CHECK (cancelled_chunk_count >= 0),
    status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT pk_run_shard_summaries PRIMARY KEY (run_id, run_shard),
    CONSTRAINT fk_run_shard_summaries_snapshot
        FOREIGN KEY (run_id, run_shard)
        REFERENCES run_snapshots(run_id, run_shard)
        ON DELETE CASCADE
);

CREATE INDEX idx_run_shard_summaries_run_status
    ON run_shard_summaries(run_id, status);

COMMENT ON TABLE run_shard_summaries IS
    'Shard-local execution progress summary used by coordinators to roll up global run state without scanning all execution rows in control storage.';

COMMENT ON COLUMN run_shard_summaries.expected_execution_count IS
    'Expected execution count for this run shard, copied from the local run snapshot.';

COMMENT ON COLUMN run_shard_summaries.terminal_execution_count IS
    'Executions in this run shard that reached a terminal execution_status.';

COMMENT ON COLUMN run_shard_summaries.missing_aggregate_count IS
    'Terminal executions missing the current-attempt aggregate required for final scoring.';

COMMENT ON COLUMN run_shard_summaries.status IS
    'Shard summary status: running while work remains, completed when all expected executions are terminal and passing coverage checks, failed when terminal failures or missing aggregates are present.';
