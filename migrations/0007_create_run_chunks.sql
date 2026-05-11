CREATE TABLE run_chunks (
    id UUID NOT NULL,
    run_id UUID NOT NULL,
    dataset_version_id TEXT NOT NULL,
    profile_group_id TEXT NOT NULL,
    ordinal_start INTEGER NOT NULL CHECK (ordinal_start >= 0),
    ordinal_end INTEGER NOT NULL CHECK (ordinal_end > ordinal_start),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'leased', 'completed', 'failed', 'cancelled')),
    leased_until TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT fk_run_chunks_run_dataset
        FOREIGN KEY (run_id, dataset_version_id)
        REFERENCES runs(id, dataset_version_id)
        ON DELETE CASCADE,

    CONSTRAINT pk_run_chunks PRIMARY KEY (run_id, id)
) PARTITION BY HASH (run_id);

DO $$
DECLARE
    partition_index INTEGER;
BEGIN
    FOR partition_index IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE %I PARTITION OF run_chunks FOR VALUES WITH (MODULUS 32, REMAINDER %s)',
            'run_chunks_p' || lpad(partition_index::text, 2, '0'),
            partition_index
        );
    END LOOP;
END $$;

CREATE INDEX idx_run_chunks_run_status_leased_until
    ON run_chunks(run_id, status, leased_until);

COMMENT ON TABLE run_chunks IS
    'Chunk-level scheduling units for run processing, hash partitioned by run_id for run-local worker scheduling.';

COMMENT ON COLUMN run_chunks.id IS
    'Unique chunk identifier used in work dispatch and worker claiming.';

COMMENT ON COLUMN run_chunks.run_id IS
    'Owning run for this chunk.';

COMMENT ON COLUMN run_chunks.dataset_version_id IS
    'Dataset version identifier used to resolve chunk case membership.';

COMMENT ON COLUMN run_chunks.profile_group_id IS
    'Resolved profile group identifier associated with this chunk.';

COMMENT ON COLUMN run_chunks.ordinal_start IS
    'Inclusive starting dataset ordinal for this chunk.';

COMMENT ON COLUMN run_chunks.ordinal_end IS
    'Exclusive ending dataset ordinal for this chunk.';

COMMENT ON COLUMN run_chunks.status IS
    'Chunk processing lifecycle status.';

COMMENT ON COLUMN run_chunks.leased_until IS
    'Lease expiration timestamp for worker ownership of this chunk.';

COMMENT ON COLUMN run_chunks.created_at IS
    'Timestamp when this chunk was created.';

COMMENT ON COLUMN run_chunks.updated_at IS
    'Timestamp of the last state update for this chunk.';
