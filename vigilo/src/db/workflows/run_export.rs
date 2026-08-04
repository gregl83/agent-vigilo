//! Routed run export read helpers.
//!
//! Export reads execution-owned rows from the database placement that owns each
//! `run_id + run_shard` route. The CLI owns output formatting; this module owns
//! routing and SQL pagination.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    context::database,
    models::{
        evaluator_result::EvaluatorResult,
        execution::Execution,
        execution_aggregate::ExecutionAggregate,
        execution_attempt::ExecutionAttempt,
    },
};

mod queries;

#[derive(Debug)]
pub(crate) struct RunExportBatch {
    pub(crate) executions: Vec<Execution>,
    pub(crate) attempts: Vec<ExecutionAttempt>,
    pub(crate) aggregates: Vec<ExecutionAggregate>,
    pub(crate) evaluator_results: Vec<EvaluatorResult>,
}

#[derive(Debug, Clone)]
pub(crate) struct RunExportRoute {
    run_shard: i16,
    database_alias: String,
    db: PgPool,
}

impl RunExportRoute {
    pub(crate) fn run_shard(&self) -> i16 {
        self.run_shard
    }

    pub(crate) fn database_alias(&self) -> &str {
        &self.database_alias
    }
}

pub(crate) async fn select_run_export_routes(
    database_router: &database::DatabaseRouter,
    run_id: Uuid,
) -> anyhow::Result<Vec<RunExportRoute>> {
    let routes = database_router
        .execution_read_routes_for_run(run_id)
        .await?
        .into_iter()
        .map(|(run_shard, database_alias, db)| RunExportRoute {
            run_shard,
            database_alias,
            db,
        })
        .collect();

    Ok(routes)
}

pub(crate) async fn select_execution_batch(
    route: &RunExportRoute,
    run_id: Uuid,
    after_execution_id: Option<Uuid>,
    limit: i64,
) -> anyhow::Result<Vec<Execution>> {
    queries::select_executions(
        &route.db,
        run_id,
        route.run_shard,
        after_execution_id,
        limit,
    )
    .await
}

pub(crate) async fn select_batch_for_executions(
    route: &RunExportRoute,
    run_id: Uuid,
    executions: Vec<Execution>,
) -> anyhow::Result<RunExportBatch> {
    if executions.is_empty() {
        return Ok(RunExportBatch {
            executions,
            attempts: Vec::new(),
            aggregates: Vec::new(),
            evaluator_results: Vec::new(),
        });
    }

    let execution_ids = executions
        .iter()
        .map(|execution| execution.id)
        .collect::<Vec<_>>();

    let attempts =
        queries::select_attempts(&route.db, run_id, route.run_shard, &execution_ids).await?;
    let aggregates =
        queries::select_aggregates(&route.db, run_id, route.run_shard, &execution_ids).await?;

    let attempt_ids = attempts
        .iter()
        .map(|attempt| attempt.id)
        .collect::<Vec<_>>();

    let evaluator_results = if attempt_ids.is_empty() {
        Vec::new()
    } else {
        queries::select_evaluator_results(&route.db, run_id, route.run_shard, &attempt_ids).await?
    };

    Ok(RunExportBatch {
        executions,
        attempts,
        aggregates,
        evaluator_results,
    })
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    #[tokio::test]
    async fn empty_execution_batch_does_not_touch_the_route_database() {
        let route = closed_route().await;

        let batch = select_batch_for_executions(&route, Uuid::nil(), Vec::new())
            .await
            .unwrap();

        assert!(batch.executions.is_empty());
        assert!(batch.attempts.is_empty());
        assert!(batch.aggregates.is_empty());
        assert!(batch.evaluator_results.is_empty());
        assert_eq!(route.run_shard(), 7);
        assert_eq!(route.database_alias(), "unavailable");
    }

    #[tokio::test]
    async fn execution_page_propagates_an_unavailable_database() {
        let route = closed_route().await;

        let error = select_execution_batch(&route, Uuid::nil(), None, 100)
            .await
            .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<sqlx::Error>(),
            Some(sqlx::Error::PoolClosed)
        ));
    }

    async fn closed_route() -> RunExportRoute {
        let db = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/vigilo")
            .unwrap();
        db.close().await;
        RunExportRoute {
            run_shard: 7,
            database_alias: "unavailable".to_string(),
            db,
        }
    }
}
