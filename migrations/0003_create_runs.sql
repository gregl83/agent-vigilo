CREATE TABLE runs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    -- external/run-level identity
    run_key TEXT NOT NULL UNIQUE,

    -- human/lookup metadata
    name TEXT,
    description TEXT,

    -- what is being evaluated
    dataset_id TEXT NOT NULL,
    dataset_version TEXT NOT NULL,
    evaluation_profile_id TEXT NOT NULL,
    evaluation_profile_version TEXT NOT NULL,
    aggregation_policy_id TEXT NOT NULL,
    aggregation_policy_version TEXT NOT NULL,

    -- model + prompt/config identity
    model_provider TEXT NOT NULL,
    model_name TEXT NOT NULL,
    model_version TEXT,
    prompt_config_id TEXT NOT NULL,
    prompt_config_version TEXT NOT NULL,

    -- frozen config snapshot for reproducibility
    config_snapshot JSONB NOT NULL DEFAULT '{}'::jsonb,

    -- orchestration state
    status run_status NOT NULL DEFAULT 'pending',
    gate_status gate_status NOT NULL DEFAULT 'unknown',

    -- optional run-scoped coordinator ownership
    coordinator_id TEXT,
    coordinator_leased_until TIMESTAMPTZ,
    coordinator_heartbeat_at TIMESTAMPTZ,

    -- counts cached for reporting; source of truth is execution state
    expected_execution_count INTEGER NOT NULL DEFAULT 0 CHECK (expected_execution_count >= 0),
    terminal_execution_count INTEGER NOT NULL DEFAULT 0 CHECK (terminal_execution_count >= 0),
    passed_execution_count INTEGER NOT NULL DEFAULT 0 CHECK (passed_execution_count >= 0),
    failed_execution_count INTEGER NOT NULL DEFAULT 0 CHECK (failed_execution_count >= 0),
    errored_execution_count INTEGER NOT NULL DEFAULT 0 CHECK (errored_execution_count >= 0),

    -- summary payload for quick read/display
    summary JSONB NOT NULL DEFAULT '{}'::jsonb,

    error_message TEXT,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at TIMESTAMPTZ,
    dispatched_at TIMESTAMPTZ,
    finalized_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_runs_status ON runs(status);
CREATE INDEX idx_runs_gate_status ON runs(gate_status);
CREATE INDEX idx_runs_dataset ON runs(dataset_id, dataset_version);
CREATE INDEX idx_runs_coordinator_lease ON runs(coordinator_leased_until)
    WHERE status IN ('pending', 'running', 'finalizing');
