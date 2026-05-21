CREATE TABLE outbox_events (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    event_type TEXT NOT NULL,
    aggregate_type TEXT NOT NULL,
    aggregate_id UUID NOT NULL,

    -- useful for run.completed dedupe, etc.
    dedupe_key TEXT NOT NULL UNIQUE,

    payload JSONB NOT NULL DEFAULT '{}'::jsonb,

    status outbox_status NOT NULL DEFAULT 'pending',
    available_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    claim_token UUID,
    claimed_until TIMESTAMPTZ,
    publish_attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (publish_attempt_count >= 0),
    published_at TIMESTAMPTZ,
    error_message TEXT,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_outbox_events_status_available_at
    ON outbox_events(status, available_at);

CREATE INDEX idx_outbox_events_pending_available
    ON outbox_events(available_at, id)
    WHERE status = 'pending';

CREATE INDEX idx_outbox_events_claim_token
    ON outbox_events(id, claim_token)
    WHERE status = 'pending' AND claim_token IS NOT NULL;

CREATE INDEX idx_outbox_events_aggregate
    ON outbox_events(aggregate_type, aggregate_id);

COMMENT ON INDEX idx_outbox_events_pending_available IS
    'Hot partial index for high-throughput outbox publishers claiming pending events by availability time.';

COMMENT ON TABLE outbox_events IS
    'Outbox table used to ensure reliable, at-least-once delivery of domain events. Events are written transactionally with state changes and later published asynchronously to external systems.';

COMMENT ON COLUMN outbox_events.id IS
    'Unique identifier for the outbox event record.';

COMMENT ON COLUMN outbox_events.event_type IS
    'Type of event being emitted (e.g., run.completed). Used by consumers to interpret the payload.';

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

COMMENT ON COLUMN outbox_events.claim_token IS
    'Opaque token assigned when a publisher claims the event. Mark-published and retry updates must present the same token so stale publishers cannot overwrite a newer claim.';

COMMENT ON COLUMN outbox_events.claimed_until IS
    'Lease deadline for the current publisher claim. Mirrors the temporary availability delay used by the hot pending-event claim path.';

COMMENT ON COLUMN outbox_events.publish_attempt_count IS
    'Number of times this event has been claimed for publication.';

COMMENT ON COLUMN outbox_events.published_at IS
    'Timestamp when the event was successfully delivered to the external system.';

COMMENT ON COLUMN outbox_events.error_message IS
    'Error message from the most recent failed publication attempt, if any.';

COMMENT ON COLUMN outbox_events.created_at IS
    'Timestamp when the event was created and persisted.';

COMMENT ON COLUMN outbox_events.updated_at IS
    'Timestamp of the last update to the event record, including status changes.';
