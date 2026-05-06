CREATE TABLE case_blobs (
    case_hash TEXT PRIMARY KEY,
    task_type TEXT NOT NULL,
    input_payload JSONB NOT NULL,
    expected_output JSONB NOT NULL,
    context_payload JSONB NOT NULL DEFAULT 'null'::jsonb,
    tags JSONB NOT NULL DEFAULT '[]'::jsonb,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

COMMENT ON TABLE case_blobs IS
    'Content-addressed storage for canonical dataset case payloads. A blob is immutable and deduplicated by hash.';

COMMENT ON COLUMN case_blobs.case_hash IS
    'Deterministic content hash for the canonical case payload. Serves as immutable primary key for deduplication.';

COMMENT ON COLUMN case_blobs.task_type IS
    'Canonical task type for the case blob. Included in the content hash for comparability and routing reproducibility.';

COMMENT ON COLUMN case_blobs.input_payload IS
    'Canonicalized case input payload used by worker processing. Shared across dataset versions when unchanged.';

COMMENT ON COLUMN case_blobs.expected_output IS
    'Canonicalized expected output payload associated with the input payload hash.';

COMMENT ON COLUMN case_blobs.context_payload IS
    'Canonicalized optional case context payload associated with this content hash.';

COMMENT ON COLUMN case_blobs.tags IS
    'Canonicalized case tags array associated with this content hash.';

COMMENT ON COLUMN case_blobs.metadata IS
    'Canonicalized case metadata payload associated with this content hash.';

COMMENT ON COLUMN case_blobs.created_at IS
    'Timestamp when this blob was first inserted into the content-addressed store.';
