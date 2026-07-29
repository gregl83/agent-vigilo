CREATE TABLE shard_placements (
    run_id UUID NOT NULL,
    run_shard SMALLINT NOT NULL CHECK (run_shard >= 0 AND run_shard < 128),
    database_alias TEXT NOT NULL REFERENCES database_placements(alias),
    status TEXT NOT NULL CHECK (status IN ('active', 'moving', 'draining')),
    move_target_database_alias TEXT REFERENCES database_placements(alias),
    route_version BIGINT NOT NULL DEFAULT 1 CHECK (route_version > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT pk_shard_placements PRIMARY KEY (run_id, run_shard),
    CONSTRAINT ck_shard_placements_move_target_lifecycle CHECK (
        (status = 'active' AND move_target_database_alias IS NULL)
        OR (
            status IN ('draining', 'moving')
            AND move_target_database_alias IS NOT NULL
            AND move_target_database_alias <> database_alias
        )
    )
);

CREATE INDEX idx_shard_placements_database_alias_status
    ON shard_placements(database_alias, status);

COMMENT ON TABLE shard_placements IS
    'Routing catalog for a run logical shard. Each row maps one run_id and run_shard pair to a database placement alias.';

COMMENT ON COLUMN shard_placements.run_id IS
    'Run whose logical shard is routed by this placement row.';

COMMENT ON COLUMN shard_placements.run_shard IS
    'Logical shard number for this run. Values are constrained to the 128 shard range used by run_chunks and execution tables.';

COMMENT ON COLUMN shard_placements.database_alias IS
    'Database placement alias that owns this run shard.';

COMMENT ON COLUMN shard_placements.status IS
    'Shard placement lifecycle. Active is dispatchable, draining rejects new work while admitted work finishes, and moving freezes the source while rows are reconciled and copied.';

COMMENT ON COLUMN shard_placements.move_target_database_alias IS
    'Intended target while a shard is draining or moving. The move workflow persists this before draining and clears it only when fenced target activation succeeds.';

COMMENT ON CONSTRAINT ck_shard_placements_move_target_lifecycle ON shard_placements IS
    'Active routes cannot retain a move target. Draining and moving routes require a target distinct from the current owner.';

COMMENT ON COLUMN shard_placements.route_version IS
    'Monotonic fencing token incremented whenever the route alias or lifecycle changes. Cached routes are valid only while alias, status, and route_version still match control metadata.';

COMMENT ON INDEX idx_shard_placements_database_alias_status IS
    'Lookup index for routing and administrative scans grouped by database placement and dispatchability status.';
