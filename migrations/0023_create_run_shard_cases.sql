CREATE TABLE run_shard_cases (
    run_id UUID NOT NULL,
    run_shard SMALLINT NOT NULL CHECK (run_shard >= 0 AND run_shard < 128),
    dataset_version_id UUID NOT NULL,
    case_id UUID NOT NULL,
    case_ordinal INTEGER NOT NULL CHECK (case_ordinal >= 0),
    case_hash TEXT NOT NULL REFERENCES case_blobs(case_hash),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT fk_run_shard_cases_run_dataset
        FOREIGN KEY (run_id, dataset_version_id)
        REFERENCES runs(id, dataset_version_id)
        ON DELETE CASCADE,
    CONSTRAINT pk_run_shard_cases
        PRIMARY KEY (run_id, run_shard, case_id),
    CONSTRAINT uq_run_shard_cases_ordinal
        UNIQUE (run_id, run_shard, case_ordinal)
) PARTITION BY LIST (run_shard);

DO $$
DECLARE
    partition_index INTEGER;
BEGIN
    FOR partition_index IN 0..127 LOOP
        EXECUTE format(
            'CREATE TABLE %I PARTITION OF run_shard_cases FOR VALUES IN (%s)',
            'run_shard_cases_p' || lpad(partition_index::text, 3, '0'),
            partition_index
        );
    END LOOP;
END $$;

CREATE INDEX idx_run_shard_cases_case_hash
    ON run_shard_cases(case_hash);

COMMENT ON TABLE run_shard_cases IS
    'Immutable run-scoped case membership copied only to the execution database that owns each logical run shard.';

COMMENT ON COLUMN run_shard_cases.case_ordinal IS
    'Canonical dataset ordinal used for bounded creation pages and worker chunk range reads.';

COMMENT ON COLUMN run_shard_cases.case_hash IS
    'Content-addressed payload reference. Projection rows move with a shard while blob cleanup remains a separate retention concern.';
