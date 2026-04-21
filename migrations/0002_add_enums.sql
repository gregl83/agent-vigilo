CREATE TYPE run_status AS ENUM (
  'pending',
  'running',
  'finalizing',
  'completed',
  'failed',
  'cancelled'
);

CREATE TYPE gate_status AS ENUM (
  'unknown',
  'pass',
  'fail'
);

CREATE TYPE execution_status AS ENUM (
  'pending',
  'running',
  'awaiting_evaluators',
  'retry_scheduled',
  'completed',
  'failed',
  'timed_out',
  'cancelled'
);

CREATE TYPE attempt_status AS ENUM (
  'pending',
  'running',
  'completed',
  'failed_model_call',
  'failed_evaluation',
  'timed_out',
  'cancelled',
  'stale'
);

CREATE TYPE evaluation_status AS ENUM (
  'passed',
  'failed',
  'error',
  'skipped'
);

CREATE TYPE severity AS ENUM (
  'none',
  'low',
  'medium',
  'high',
  'critical'
);

CREATE TYPE outbox_status AS ENUM (
  'pending',
  'published',
  'failed'
);
