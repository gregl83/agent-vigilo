CREATE TABLE shard_rebalance_operations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    strategy TEXT NOT NULL CHECK (strategy IN ('drain-source', 'fill-target')),
    source_database_alias TEXT REFERENCES database_placements(alias),
    target_database_alias TEXT NOT NULL REFERENCES database_placements(alias),
    status TEXT NOT NULL DEFAULT 'planned'
        CHECK (status IN ('planned', 'running', 'completed', 'cancelled', 'failed')),

    planned_item_count INTEGER NOT NULL DEFAULT 0 CHECK (planned_item_count >= 0),
    completed_item_count INTEGER NOT NULL DEFAULT 0 CHECK (completed_item_count >= 0),
    failed_item_count INTEGER NOT NULL DEFAULT 0 CHECK (failed_item_count >= 0),
    cancelled_item_count INTEGER NOT NULL DEFAULT 0 CHECK (cancelled_item_count >= 0),

    error_message TEXT,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    cancelled_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_shard_rebalance_operations_status_created_at
    ON shard_rebalance_operations(status, created_at);

CREATE TABLE shard_rebalance_items (
    operation_id UUID NOT NULL REFERENCES shard_rebalance_operations(id) ON DELETE CASCADE,
    sequence_no INTEGER NOT NULL CHECK (sequence_no >= 0),

    run_id UUID NOT NULL,
    run_shard SMALLINT NOT NULL CHECK (run_shard >= 0 AND run_shard < 128),
    source_database_alias TEXT NOT NULL REFERENCES database_placements(alias),
    target_database_alias TEXT NOT NULL REFERENCES database_placements(alias),
    planned_route_version BIGINT NOT NULL CHECK (planned_route_version > 0),

    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'running', 'completed', 'failed', 'cancelled')),
    error_message TEXT,

    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT pk_shard_rebalance_items PRIMARY KEY (operation_id, run_id, run_shard),
    CONSTRAINT uq_shard_rebalance_items_sequence UNIQUE (operation_id, sequence_no)
);

CREATE INDEX idx_shard_rebalance_items_operation_status_sequence
    ON shard_rebalance_items(operation_id, status, sequence_no);

COMMENT ON TABLE shard_rebalance_operations IS
    'Control-plane ledger for bulk shard rebalance plans. One operation records the target placement, strategy, status, and aggregate progress for resumable movement.';

COMMENT ON COLUMN shard_rebalance_operations.strategy IS
    'Planning strategy. drain-source moves shards away from a named source placement; fill-target moves excess shards from loaded placements into the target placement.';

COMMENT ON COLUMN shard_rebalance_operations.source_database_alias IS
    'Optional source placement. Required by drain-source and omitted by fill-target.';

COMMENT ON COLUMN shard_rebalance_operations.target_database_alias IS
    'Shard-capable placement that receives planned rebalance items.';

COMMENT ON COLUMN shard_rebalance_operations.status IS
    'Operation lifecycle. Planned and running can be applied; completed, cancelled, and failed are terminal.';

COMMENT ON TABLE shard_rebalance_items IS
    'Per-run-shard item ledger for a bulk rebalance operation. Apply uses the existing single-shard move workflow for each pending item and records progress here.';

COMMENT ON COLUMN shard_rebalance_items.planned_route_version IS
    'Route fencing value observed when the item was planned. Apply verifies the route still matches before moving the shard.';

COMMENT ON COLUMN shard_rebalance_items.status IS
    'Item lifecycle. Pending items can be applied, running is a transient claim state, completed moved successfully, failed records the last error, and cancelled was skipped by operator cancellation.';
