CREATE TABLE dataset_versions (
    dataset_version_id TEXT PRIMARY KEY,
    dataset_id TEXT NOT NULL,
    dataset_version TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

COMMENT ON TABLE dataset_versions IS
    'Canonical catalog of resolved dataset versions used by runs and membership rows.';

COMMENT ON COLUMN dataset_versions.dataset_version_id IS
    'Deterministic identifier for a dataset membership/version snapshot.';

COMMENT ON COLUMN dataset_versions.dataset_id IS
    'Logical dataset identifier associated with this resolved dataset version.';

COMMENT ON COLUMN dataset_versions.dataset_version IS
    'Source dataset version string associated with this resolved dataset version id.';

COMMENT ON COLUMN dataset_versions.created_at IS
    'Timestamp when this dataset version row was first inserted.';

COMMENT ON COLUMN dataset_versions.updated_at IS
    'Timestamp of the last dataset version identity update.';

