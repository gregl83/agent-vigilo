CREATE TABLE dataset_version_cases (
    dataset_version_id TEXT NOT NULL,
    case_id TEXT NOT NULL,
    case_ordinal INTEGER NOT NULL CHECK (case_ordinal >= 0),
    case_hash TEXT NOT NULL REFERENCES case_blobs(case_hash),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (dataset_version_id, case_id),
    UNIQUE (dataset_version_id, case_ordinal)
);

CREATE INDEX idx_dataset_version_cases_version_ordinal
    ON dataset_version_cases(dataset_version_id, case_ordinal);

COMMENT ON TABLE dataset_version_cases IS
    'Dataset-version membership table mapping stable logical case ids to immutable case blobs and deterministic ordinals.';

COMMENT ON COLUMN dataset_version_cases.dataset_version_id IS
    'Identifier for a resolved dataset version defined by case membership.';

COMMENT ON COLUMN dataset_version_cases.case_id IS
    'Stable logical case identifier within a dataset family.';

COMMENT ON COLUMN dataset_version_cases.case_ordinal IS
    'Deterministic case order within a dataset version, used for chunk range dispatch.';

COMMENT ON COLUMN dataset_version_cases.case_hash IS
    'Reference to immutable case content in case_blobs.';

COMMENT ON COLUMN dataset_version_cases.created_at IS
    'Timestamp when this dataset-version membership row was created.';

COMMENT ON COLUMN dataset_version_cases.updated_at IS
    'Timestamp of the last update to this dataset-version membership row.';
