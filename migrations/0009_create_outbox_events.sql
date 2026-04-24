CREATE TABLE outbox_events (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    event_type TEXT NOT NULL,
    aggregate_type TEXT NOT NULL,
    aggregate_id UUID NOT NULL,

    -- useful for RunCompleted dedupe, etc.
    dedupe_key TEXT NOT NULL UNIQUE,

    payload JSONB NOT NULL DEFAULT '{}'::jsonb,

    status outbox_status NOT NULL DEFAULT 'pending',
    available_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    published_at TIMESTAMPTZ,
    error_message TEXT,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_outbox_events_status_available_at
    ON outbox_events(status, available_at);

CREATE INDEX idx_outbox_events_aggregate
    ON outbox_events(aggregate_type, aggregate_id);

COMMENT ON TABLE outbox_events IS
    'Outbox table used to ensure reliable, at-least-once delivery of domain events. Events are written transactionally with state changes and later published asynchronously to external systems.';

COMMENT ON COLUMN outbox_events.id IS
    'Unique identifier for the outbox event record.';

COMMENT ON COLUMN outbox_events.event_type IS
    'Type of event being emitted (e.g., RunCompleted). Used by consumers to interpret the payload.';

COMMENT ON COLUMN outbox_events.aggregate_type IS
    'Type of aggregate that produced the event (e.g., run). Used for routing and grouping.';

COMMENT ON COLUMN outbox_events.aggregate_id IS
    'Identifier of the aggregate instance associated with this event (e.g., run_id).';

COMMENT ON COLUMN outbox_events.dedupe_key IS
    'Idempotency key used to prevent duplicate event processing. Typically derived from aggregate identity and event type.';

COMMENT ON COLUMN outbox_events.payload IS
    'Serialized event payload containing relevant data for consumers. Structure depends on event_type.';

COMMENT ON COLUMN outbox_events.status IS
    'Current publication state of the event. Used by the outbox publisher to track delivery progress and retries.';

COMMENT ON COLUMN outbox_events.available_at IS
    'Timestamp indicating when the event is eligible for publication. Used for delayed delivery or retry backoff.';

COMMENT ON COLUMN outbox_events.published_at IS
    'Timestamp when the event was successfully delivered to the external system.';

COMMENT ON COLUMN outbox_events.error_message IS
    'Error message from the most recent failed publication attempt, if any.';

COMMENT ON COLUMN outbox_events.created_at IS
    'Timestamp when the event was created and persisted.';

COMMENT ON COLUMN outbox_events.updated_at IS
    'Timestamp of the last update to the event record, including status changes.';
