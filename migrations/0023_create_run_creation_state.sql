CREATE TABLE run_creation_placements (
    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    database_alias TEXT NOT NULL REFERENCES database_placements(alias),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'seeded', 'failed')),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    expected_case_count BIGINT NOT NULL CHECK (expected_case_count >= 0),
    seeded_case_count BIGINT NOT NULL DEFAULT 0 CHECK (seeded_case_count >= 0),
    last_seeded_case_ordinal INTEGER CHECK (last_seeded_case_ordinal >= 0),
    case_projection_hash TEXT NOT NULL,
    last_error TEXT,
    seeded_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, database_alias),
    CHECK (seeded_case_count <= expected_case_count),
    CHECK (status <> 'seeded' OR seeded_at IS NOT NULL)
);

CREATE TABLE run_creation_chunks (
    run_id UUID NOT NULL,
    database_alias TEXT NOT NULL,
    chunk_id UUID NOT NULL,
    run_shard SMALLINT NOT NULL CHECK (run_shard >= 0 AND run_shard < 128),
    profile_group_id TEXT NOT NULL,
    ordinal_start INTEGER NOT NULL CHECK (ordinal_start >= 0),
    ordinal_end INTEGER NOT NULL CHECK (ordinal_end > ordinal_start),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, run_shard, chunk_id),
    FOREIGN KEY (run_id, database_alias)
        REFERENCES run_creation_placements(run_id, database_alias)
        ON DELETE CASCADE
);

CREATE INDEX idx_run_creation_chunks_placement
    ON run_creation_chunks(run_id, database_alias, run_shard, ordinal_start, chunk_id);

COMMENT ON TABLE run_creation_placements IS
    'Control-plane ledger for execution databases that must be seeded before a creating run can become dispatchable.';

COMMENT ON COLUMN run_creation_placements.status IS
    'Seed lifecycle for one execution database: pending is retryable, seeded is complete, and failed is terminal.';

COMMENT ON COLUMN run_creation_placements.attempt_count IS
    'Number of seed attempts started for this run and database placement.';

COMMENT ON COLUMN run_creation_placements.last_seeded_case_ordinal IS
    'Last projection ordinal acknowledged after its execution-database transaction committed; count and hash verification determine completion.';

COMMENT ON COLUMN run_creation_placements.case_projection_hash IS
    'BLAKE3 digest of the ordered immutable shard-case assignment expected on this execution placement.';

COMMENT ON COLUMN run_creation_placements.last_error IS
    'Most recent seed failure retained for recovery diagnostics.';

COMMENT ON TABLE run_creation_chunks IS
    'Temporary control-plane chunk plan used to reproduce exact execution seed writes while a run is creating.';

COMMENT ON COLUMN run_creation_chunks.database_alias IS
    'Execution database selected for this chunk by the persisted shard assignment policy.';
