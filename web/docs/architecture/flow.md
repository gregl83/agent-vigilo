---
title: Execution Flow & State Model
---

# Execution Flow & State Model

This document describes how Agent Vigilo processes an evaluation run from invocation to completion, including execution lifecycle, retry behavior, and event publication.

---

## Overview

At a high level, the system follows a fan-out / fan-in pattern:

1. A **run** is created
2. **executions** are generated from a dataset
3. workers process executions via **attempts**
4. evaluators produce append-only results
5. execution aggregates are computed
6. the run is finalized
7. a completion event is published

---

## Core Concepts

### Run
A run represents a batch evaluation of a versioned agent target against a dataset and evaluation profile.

### Execution
An execution represents one dataset case evaluated against the target system.

### Attempt
An attempt represents a single worker’s effort to process an execution. Multiple attempts may exist due to retries or failures.

### Evaluator Result
An append-only record representing the outcome of a single evaluator.

---

## End-to-End Flow

### 1. Run Creation

A run is created with:

- dataset
- evaluation profile
- aggregation policy
- agent configuration (versioned target)

```text
run.status = pending
run.gate_status = unknown
```

Executions are then generated:

```text
execution.status = pending
```

### 2. Execution Dispatch

Workers claim executions:

```text
execution.status = running
execution_attempt.status = running
```

Each attempt is associated with:

- a worker
- a lease (for failure detection)
- an attempt number

### 3. Agent Invocation

The worker invokes the configured agent target:

- may be a single model call
- may be a multi-step workflow

If the agent call fails:

```text
attempt.status = failed_agent_call
execution.status = retry_scheduled (if retryable)
```

### 4. Evaluation Phase

After a successful agent response:

- evaluators are executed
- results are appended to evaluator_results

Each evaluator produces:

- status (passed / failed / error / skipped)
- severity
- normalized score
- evidence

If evaluation fails:

```text
attempt.status = failed_evaluation
execution.status = retry_scheduled (if retryable)
```

### 5. Execution Completion

Once all evaluator results are persisted:

- execution aggregate is computed
- execution is marked terminal

```text
attempt.status = completed
execution.status = completed | failed | timed_out
```

### 6. Retry Flow

If an execution fails but is retryable:

```text
execution.status = retry_scheduled
```

A new attempt is created:

```text
attempt.status = pending → running
```

Older attempts may become:

```text
attempt.status = stale
```

This occurs when:

- a worker loses its lease
- a newer attempt supersedes it

### 7. Run Finalization

Workers (or a coordinator) check:

> Are there any non-terminal executions remaining?

If no:

```text
run.status = finalizing
```

The system:

- aggregates execution results
- computes run summary
- determines gate_status

```text
run.status = completed
run.gate_status = pass | fail
```

### 8. Event Publication (Outbox Pattern)

A RunCompleted event is inserted into the outbox:

```text
outbox.status = pending
```

A publisher process:

```text
pending → published
        ↘ failed (retry)
```

This ensures reliable, at-least-once delivery.

## State Machines

### Run Lifecycle

```text
pending → running → finalizing → completed
                  ↘ failed
                  ↘ cancelled
```

### Execution Lifecycle

```text
pending → running → completed
                  ↘ retry_scheduled → running
                  ↘ failed
                  ↘ timed_out
                  ↘ cancelled
```

### Attempt Lifecycle

```text
pending → running → completed
                  ↘ failed_agent_call
                  ↘ failed_evaluation
                  ↘ timed_out
                  ↘ cancelled
                  ↘ stale
```

## Key Design Properties

### 1. Append-only evaluator results

Evaluator outputs are never updated, only inserted. This provides:

- auditability
- reproducibility
- traceability

### 2. Separation of state vs evidence

- state tables (runs, executions, attempts) are mutable
- evaluator results are immutable facts

### 3. Idempotent finalization

Multiple workers may attempt to finalize a run.

The system ensures:

- only one finalization succeeds
- duplicate attempts are safe

### 4. Retry-safe execution

Executions may have multiple attempts.

Only the most recent non-stale attempt is authoritative.

### 5. Reliable event delivery

The outbox pattern ensures:

- no lost events
- retryable publishing
- eventual consistency

## Design Philosophy

Agent Vigilo evaluates the behavior of a target system, not just a model.

An "agent" may represent:

- a single model call
- a prompt pipeline
- a multi-step workflow
- a deployed HTTP service

The evaluation system treats all targets uniformly via a versioned invocation interface.

## Summary

The system is designed to:

- handle distributed execution safely
- tolerate worker failure and retries
- preserve evaluation evidence
- produce deterministic, policy-driven outcomes
- reliably signal completion to downstream systems
