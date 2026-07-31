//! Lazy PostgreSQL pool router and placement configuration context.
//!
//! Commands call this module through
//! [`crate::context::Context::dbr`] when they first need database
//! access. This context initializes the database router once, then the router
//! lazily initializes its control pool and per-placement pools. Connection
//! parameters remain fixed for the process lifetime, while placement status
//! and role are read from the control database before new work is admitted.
//! Environment variables only provide secret connection values.
//!
//! Keep topology choices behind database workflow boundaries. Thin command dispatch,
//! generic runtime code, and arbitrary domain callers should prefer workflows
//! over directly choosing the control or execution database.

use std::{
    collections::HashMap,
    time::{
        Duration,
        Instant,
    },
};

use moka::future::Cache;
use sqlx::{
    PgPool,
    Postgres,
    Transaction,
    postgres::PgPoolOptions,
};
use tokio::sync::OnceCell;
use tracing::debug;
use uuid::Uuid;

use crate::{
    db::tables::{
        database_placements,
        shard_placements,
    },
    models::{
        database_placement::{
            DATABASE_PLACEMENT_STATUS_ACTIVE,
            DEFAULT_DATABASE_URL_ENV,
            DatabasePlacement,
        },
        shard_placement::{
            SHARD_PLACEMENT_STATUS_MOVING,
            ShardPlacement,
        },
    },
};

const SHARD_PLACEMENT_CACHE_TTL: Duration = Duration::from_secs(5);
const SHARD_PLACEMENT_CACHE_CAPACITY: u64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ExecutionRouteError {
    #[error(
        "missing shard placement for run {run_id} shard {run_shard}; run creation must insert shard_placements rows before execution routing"
    )]
    MissingShardPlacement { run_id: Uuid, run_shard: i16 },
    #[error(
        "shard placement for run {run_id} shard {run_shard} has status {status}, which is not dispatchable"
    )]
    NonDispatchableShardPlacement {
        run_id: Uuid,
        run_shard: i16,
        status: String,
    },
    #[error(
        "stale execution route for run {run_id} shard {run_shard}; expected {expected_database_alias} version {expected_route_version}, found {actual_database_alias} status {actual_status} version {actual_route_version}"
    )]
    StaleExecutionRoute {
        run_id: Uuid,
        run_shard: i16,
        expected_database_alias: String,
        expected_route_version: i64,
        actual_database_alias: String,
        actual_status: String,
        actual_route_version: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShardAssignmentPolicy {
    SingleDefault,
    SpreadActive,
}

impl ShardAssignmentPolicy {
    pub(crate) fn parse(raw: &str) -> anyhow::Result<Self> {
        match normalize_alias(raw.to_string(), "shard assignment policy")?.as_str() {
            "single-default" => Ok(Self::SingleDefault),
            "spread-active" => Ok(Self::SpreadActive),
            other => anyhow::bail!(
                "unsupported shard assignment policy {}; expected single-default or spread-active",
                other
            ),
        }
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::SingleDefault => "single-default",
            Self::SpreadActive => "spread-active",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlacementConfig {
    pub(crate) control_database_alias: String,
    pub(crate) default_shard_database_alias: String,
    pub(crate) shard_assignment_policy: ShardAssignmentPolicy,
}

impl PlacementConfig {
    pub(crate) fn new(
        control_database_alias: String,
        default_shard_database_alias: String,
        shard_assignment_policy: String,
    ) -> anyhow::Result<Self> {
        let control_database_alias = normalize_alias(control_database_alias, "control alias")?;
        let default_shard_database_alias =
            normalize_alias(default_shard_database_alias, "default shard alias")?;
        let shard_assignment_policy = ShardAssignmentPolicy::parse(&shard_assignment_policy)?;

        Ok(Self {
            control_database_alias,
            default_shard_database_alias,
            shard_assignment_policy,
        })
    }

    #[cfg(test)]
    pub(crate) fn default_single_database() -> Self {
        Self {
            control_database_alias: crate::models::database_placement::DEFAULT_DATABASE_ALIAS
                .to_string(),
            default_shard_database_alias: crate::models::database_placement::DEFAULT_DATABASE_ALIAS
                .to_string(),
            shard_assignment_policy: ShardAssignmentPolicy::SingleDefault,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) uri: String,
    pub(crate) max_connections: u32,
    pub(crate) placement_config: PlacementConfig,
}

pub(crate) struct Context {
    pub(crate) config: Config,
    pub(crate) cell: OnceCell<DatabaseRouter>,
}

impl Context {
    pub(crate) async fn get(&self) -> anyhow::Result<&DatabaseRouter> {
        self.cell
            .get_or_try_init(|| async {
                Ok::<DatabaseRouter, anyhow::Error>(DatabaseRouter::new(self.config.clone()))
            })
            .await
    }
}

/// Resolves control and execution database pools from placement metadata.
pub(crate) struct DatabaseRouter {
    pub(crate) uri: String,
    pub(crate) max_connections: u32,
    pub(crate) placement_config: PlacementConfig,
    pub(crate) control_pool: OnceCell<PgPool>,
    #[allow(dead_code)]
    pub(crate) placement_pools: OnceCell<PlacementPools>,
    #[allow(dead_code)]
    pub(crate) shard_placement_cache: Cache<ShardPlacementKey, ShardPlacement>,
}

/// One serviceable execution route resolved from authoritative control metadata.
///
/// Write paths retain the route fence alongside the pool so admission can be
/// revalidated immediately before changing execution-owned state.
#[derive(Clone)]
pub(crate) struct ExecutionRoute {
    pub(crate) placement: ShardPlacement,
    pub(crate) pool: PgPool,
}

impl DatabaseRouter {
    fn new(config: Config) -> Self {
        Self {
            uri: config.uri,
            max_connections: config.max_connections,
            placement_config: config.placement_config,
            control_pool: OnceCell::new(),
            placement_pools: OnceCell::new(),
            shard_placement_cache: new_shard_placement_cache(),
        }
    }

    /// Returns the control database pool.
    ///
    /// The pool is initialized on first use, not when the database router is
    /// retrieved from [`crate::context::Context::dbr`].
    pub async fn control(&self) -> anyhow::Result<&PgPool> {
        let started = Instant::now();
        self.control_pool
            .get_or_try_init(|| async {
                debug!("initializing postgres database connection");

                PgPoolOptions::new()
                    .max_connections(self.max_connections)
                    .connect(&self.uri)
                    .await
                    .map_err(|error| {
                        let message = format!("database connection failed: {error}");
                        anyhow::Error::new(error).context(message)
                    })
            })
            .await
            .inspect(|_| {
                debug!(
                    database_alias = %self.placement_config.control_database_alias,
                    pool_acquisition_ms = started.elapsed().as_millis() as u64,
                    "resolved control database pool"
                );
            })
    }

    pub(crate) fn default_execution_database_alias(&self) -> &str {
        &self.placement_config.default_shard_database_alias
    }

    pub(crate) fn shard_assignment_policy(&self) -> &ShardAssignmentPolicy {
        &self.placement_config.shard_assignment_policy
    }

    pub(crate) fn control_database_alias(&self) -> &str {
        &self.placement_config.control_database_alias
    }

    pub(crate) async fn active_shard_capable_database_aliases(
        &self,
    ) -> anyhow::Result<Vec<String>> {
        let db = self.control().await?;
        database_placements::list_active_shard_capable_database_aliases(db).await
    }

    pub(crate) async fn serviceable_execution_database_aliases(
        &self,
    ) -> anyhow::Result<Vec<String>> {
        let db = self.control().await?;
        database_placements::list_serviceable_shard_database_aliases(db).await
    }

    pub(crate) async fn serviceable_outbox_database_aliases(&self) -> anyhow::Result<Vec<String>> {
        let db = self.control().await?;
        database_placements::list_serviceable_database_aliases(db).await
    }

    /// Returns a pool for a placement that may finish existing work.
    ///
    /// This is an infrastructure hook for router/admin workflows. Most callers
    /// should use a domain workflow or [`Self::execution`] instead of naming a
    /// placement directly. Active and draining placements are serviceable.
    /// Status is read from the control database on every call; connection
    /// parameters and pools remain fixed for the process lifetime.
    #[allow(dead_code)]
    pub async fn placement(&self, alias: &str) -> anyhow::Result<&PgPool> {
        let started = Instant::now();
        let alias = normalize_alias(alias.to_string(), "database alias")?;
        let placement = self.require_serviceable_placement(&alias).await?;

        self.resolve_placement_pool(&placement, started).await
    }

    /// Returns a pool for a shard-capable owner of existing execution data.
    ///
    /// Active and draining targets remain available because draining must not
    /// strand routed work, recovery, creation replay, or source-side movement.
    pub(crate) async fn execution_database(&self, alias: &str) -> anyhow::Result<&PgPool> {
        let started = Instant::now();
        let alias = normalize_alias(alias.to_string(), "database alias")?;
        let placement = self.require_serviceable_placement(&alias).await?;
        require_shard_capable_placement(&placement)?;

        self.resolve_placement_pool(&placement, started).await
    }

    /// Returns a pool for a target that may receive new shard ownership.
    pub(crate) async fn execution_target_database(&self, alias: &str) -> anyhow::Result<&PgPool> {
        let started = Instant::now();
        let alias = normalize_alias(alias.to_string(), "database alias")?;
        let placement = self.require_active_placement(&alias).await?;
        require_shard_capable_placement(&placement)?;

        self.resolve_placement_pool(&placement, started).await
    }

    async fn resolve_placement_pool(
        &self,
        placement: &DatabasePlacement,
        started: Instant,
    ) -> anyhow::Result<&PgPool> {
        let pools = self.placement_pools().await?;
        let Some(pool) = pools.pools_by_alias.get(&placement.alias) else {
            anyhow::bail!(
                "database placement alias {} has no process-local connection configuration; restart Vigilo after adding database placements",
                placement.alias
            );
        };

        if pool.database_url_env != placement.database_url_env {
            anyhow::bail!(
                "database placement alias {} changed database_url_env from {} to {}; restart Vigilo to load new connection parameters",
                placement.alias,
                pool.database_url_env,
                placement.database_url_env
            );
        }

        if placement.alias == self.placement_config.control_database_alias {
            let pool = self.control().await?;
            debug!(
                database_alias = %placement.alias,
                placement_pool_acquisition_ms = started.elapsed().as_millis() as u64,
                pool_source = "control",
                "resolved database placement pool"
            );
            return Ok(pool);
        }

        let pool = pool.get(&placement.alias, self.max_connections).await?;
        debug!(
            database_alias = %placement.alias,
            placement_pool_acquisition_ms = started.elapsed().as_millis() as u64,
            pool_source = "placement",
            "resolved database placement pool"
        );
        Ok(pool)
    }

    /// Resolves the stored execution placement for a run shard.
    #[allow(dead_code)]
    pub async fn execution_placement(
        &self,
        run_id: Uuid,
        run_shard: i16,
    ) -> anyhow::Result<ShardPlacement> {
        let placement = self.resolve_execution_placement(run_id, run_shard).await?;
        self.validate_shard_placement_alias(&placement).await?;
        Ok(placement)
    }

    async fn resolve_execution_placement(
        &self,
        run_id: Uuid,
        run_shard: i16,
    ) -> anyhow::Result<ShardPlacement> {
        let started = Instant::now();
        validate_run_shard(run_shard)?;

        let key = ShardPlacementKey { run_id, run_shard };
        if let Some(placement) = self.shard_placement_cache.get(&key).await {
            let current = self
                .select_control_shard_placement(run_id, run_shard)
                .await?;

            if placement.same_route_fence(&current) {
                debug!(
                    run_id = %run_id,
                    run_shard,
                    database_alias = %placement.database_alias,
                    placement_status = %placement.status,
                    route_version = placement.route_version,
                    route_resolution_ms = started.elapsed().as_millis() as u64,
                    route_cache = "hit",
                    routing_decision = "cached_execution_placement",
                    "resolved execution placement from route-version cache"
                );
                return Ok(placement);
            }

            self.shard_placement_cache.invalidate(&key).await;
            debug!(
                run_id = %run_id,
                run_shard,
                previous_database_alias = %placement.database_alias,
                previous_placement_status = %placement.status,
                previous_route_version = placement.route_version,
                database_alias = %current.database_alias,
                placement_status = %current.status,
                route_version = current.route_version,
                route_resolution_ms = started.elapsed().as_millis() as u64,
                route_cache = "stale_refresh",
                routing_decision = "refreshed_stale_execution_placement",
                "refreshed stale execution placement cache"
            );
            self.shard_placement_cache
                .insert(key, current.clone())
                .await;
            return Ok(current);
        }

        let placement = self
            .select_control_shard_placement(run_id, run_shard)
            .await?;

        debug!(
            run_id = %run_id,
            run_shard,
            database_alias = %placement.database_alias,
            placement_status = %placement.status,
            route_version = placement.route_version,
            route_resolution_ms = started.elapsed().as_millis() as u64,
            route_cache = "miss",
            routing_decision = "control_lookup_execution_placement",
            "resolved execution placement from control metadata"
        );
        self.shard_placement_cache
            .insert(key, placement.clone())
            .await;

        Ok(placement)
    }

    /// Returns the execution-owned database pool for a run shard.
    ///
    /// Routing is based on persisted `shard_placements` data. Callers that do
    /// not own execution data should use a workflow that hides this choice.
    #[allow(dead_code)]
    pub async fn execution(&self, run_id: Uuid, run_shard: i16) -> anyhow::Result<&PgPool> {
        let placement = self.resolve_execution_placement(run_id, run_shard).await?;

        if !placement.is_dispatchable() {
            debug!(
                run_id = %run_id,
                run_shard,
                database_alias = %placement.database_alias,
                placement_status = %placement.status,
                routing_decision = "blocked_non_dispatchable",
                "execution placement is not dispatchable"
            );
            return Err(ExecutionRouteError::NonDispatchableShardPlacement {
                run_id,
                run_shard,
                status: placement.status,
            }
            .into());
        }

        debug!(
            run_id = %run_id,
            run_shard,
            database_alias = %placement.database_alias,
            placement_status = %placement.status,
            routing_decision = "dispatchable_execution_pool",
            "resolved dispatchable execution pool"
        );
        self.execution_database(&placement.database_alias).await
    }

    /// Resolves an active execution route together with its current fence.
    pub(crate) async fn execution_route(
        &self,
        run_id: Uuid,
        run_shard: i16,
    ) -> anyhow::Result<ExecutionRoute> {
        let placement = self.resolve_execution_placement(run_id, run_shard).await?;
        if !placement.is_dispatchable() {
            return Err(ExecutionRouteError::NonDispatchableShardPlacement {
                run_id,
                run_shard,
                status: placement.status,
            }
            .into());
        }

        let pool = self
            .execution_database(&placement.database_alias)
            .await?
            .clone();
        Ok(ExecutionRoute { placement, pool })
    }

    /// Acquires write admission and revalidates a previously resolved route.
    ///
    /// Movement takes the exclusive form of the same advisory lock. Holding
    /// the shared lock from revalidation through the local write closes the
    /// gap where a route could become `moving` after pool resolution.
    pub(crate) async fn begin_execution_admission(
        &self,
        route: &ExecutionRoute,
    ) -> anyhow::Result<Transaction<'static, Postgres>> {
        let mut tx = route.pool.begin().await?;
        crate::db::shard_write_fence::lock_shared(
            &mut tx,
            route.placement.run_id,
            route.placement.run_shard,
        )
        .await?;

        let current = if route.placement.database_alias == self.control_database_alias() {
            select_shard_placement_on_connection(
                &mut tx,
                route.placement.run_id,
                route.placement.run_shard,
            )
            .await?
        } else {
            self.select_control_shard_placement(route.placement.run_id, route.placement.run_shard)
                .await?
        };

        if !route.placement.same_route_fence(&current) {
            let error = ExecutionRouteError::StaleExecutionRoute {
                run_id: current.run_id,
                run_shard: current.run_shard,
                expected_database_alias: route.placement.database_alias.clone(),
                expected_route_version: route.placement.route_version,
                actual_database_alias: current.database_alias,
                actual_status: current.status,
                actual_route_version: current.route_version,
            };
            tx.rollback().await?;
            return Err(error.into());
        }

        if !current.is_dispatchable() {
            let error = ExecutionRouteError::NonDispatchableShardPlacement {
                run_id: current.run_id,
                run_shard: current.run_shard,
                status: current.status,
            };
            tx.rollback().await?;
            return Err(error.into());
        }

        Ok(tx)
    }

    /// Acquires shared admission for cleanup across routes on one placement.
    ///
    /// Cancellation may finish cleanup while a route is `draining`, but
    /// `moving` is fully frozen for target reconciliation and copying. Locks
    /// are acquired in shard order to keep concurrent cleanup deterministic.
    pub(crate) async fn begin_execution_cleanup_admission(
        &self,
        routes: &[ExecutionRoute],
    ) -> anyhow::Result<Transaction<'static, Postgres>> {
        let Some(first) = routes.first() else {
            anyhow::bail!("execution cleanup admission requires at least one route");
        };
        if routes.iter().any(|route| {
            route.placement.run_id != first.placement.run_id
                || route.placement.database_alias != first.placement.database_alias
        }) {
            anyhow::bail!("execution cleanup admission routes must share one run and placement");
        }

        let mut ordered = routes.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|route| route.placement.run_shard);
        let mut tx = first.pool.begin().await?;
        for route in &ordered {
            crate::db::shard_write_fence::lock_shared(
                &mut tx,
                route.placement.run_id,
                route.placement.run_shard,
            )
            .await?;
        }

        for route in ordered {
            let current = if route.placement.database_alias == self.control_database_alias() {
                select_shard_placement_on_connection(
                    &mut tx,
                    route.placement.run_id,
                    route.placement.run_shard,
                )
                .await?
            } else {
                self.select_control_shard_placement(
                    route.placement.run_id,
                    route.placement.run_shard,
                )
                .await?
            };
            if !route.placement.same_route_fence(&current) {
                let error = ExecutionRouteError::StaleExecutionRoute {
                    run_id: current.run_id,
                    run_shard: current.run_shard,
                    expected_database_alias: route.placement.database_alias.clone(),
                    expected_route_version: route.placement.route_version,
                    actual_database_alias: current.database_alias,
                    actual_status: current.status,
                    actual_route_version: current.route_version,
                };
                tx.rollback().await?;
                return Err(error.into());
            }
            if current.status == SHARD_PLACEMENT_STATUS_MOVING {
                let error = ExecutionRouteError::NonDispatchableShardPlacement {
                    run_id: current.run_id,
                    run_shard: current.run_shard,
                    status: current.status,
                };
                tx.rollback().await?;
                return Err(error.into());
            }
        }

        Ok(tx)
    }

    /// Returns all dispatchable execution routes for a run.
    #[allow(dead_code)]
    pub async fn execution_routes_for_run(
        &self,
        run_id: Uuid,
    ) -> anyhow::Result<Vec<(i16, String, PgPool)>> {
        let placements = self.execution_placements_for_run(run_id).await?;
        let mut routed = Vec::with_capacity(placements.len());

        for placement in placements {
            if !placement.is_dispatchable() {
                debug!(
                    run_id = %run_id,
                    run_shard = placement.run_shard,
                    database_alias = %placement.database_alias,
                    placement_status = %placement.status,
                    routing_decision = "blocked_non_dispatchable",
                    "execution route is not dispatchable"
                );
                anyhow::bail!(
                    "shard placement for run {} shard {} has status {}, which is not dispatchable",
                    run_id,
                    placement.run_shard,
                    placement.status
                );
            }

            let pool = self
                .execution_database(&placement.database_alias)
                .await?
                .clone();
            debug!(
                run_id = %run_id,
                run_shard = placement.run_shard,
                database_alias = %placement.database_alias,
                placement_status = %placement.status,
                routing_decision = "dispatchable_execution_route",
                "resolved dispatchable execution route"
            );
            routed.push((placement.run_shard, placement.database_alias, pool));
        }

        Ok(routed)
    }

    /// Returns stored execution placement rows without resolving their pools.
    ///
    /// Placement-scoped coordinator passes use this to open and inspect each
    /// database independently, so one unavailable target does not discard
    /// successful reads from other placements.
    pub(crate) async fn execution_placements_for_run(
        &self,
        run_id: Uuid,
    ) -> anyhow::Result<Vec<ShardPlacement>> {
        let db = self.control().await?;
        shard_placements::list_shard_placements_for_run(db, run_id).await
    }

    /// Returns all readable execution routes for a run.
    ///
    /// Read paths such as results/export can read from `moving` or `draining`
    /// shard placements. Those states block new claims and dispatch, not
    /// inspection of the placement currently recorded for the shard.
    pub(crate) async fn execution_read_routes_for_run(
        &self,
        run_id: Uuid,
    ) -> anyhow::Result<Vec<(i16, String, PgPool)>> {
        let routes = self
            .execution_read_routes_with_fences_for_run(run_id)
            .await?;
        Ok(routes
            .into_iter()
            .map(|route| {
                (
                    route.placement.run_shard,
                    route.placement.database_alias,
                    route.pool,
                )
            })
            .collect())
    }

    /// Returns readable routes with their exact control-database fences.
    pub(crate) async fn execution_read_routes_with_fences_for_run(
        &self,
        run_id: Uuid,
    ) -> anyhow::Result<Vec<ExecutionRoute>> {
        let db = self.control().await?;
        let placements = shard_placements::list_shard_placements_for_run(db, run_id).await?;
        let mut routed = Vec::with_capacity(placements.len());

        for placement in placements {
            let pool = self
                .execution_database(&placement.database_alias)
                .await?
                .clone();
            debug!(
                run_id = %run_id,
                run_shard = placement.run_shard,
                database_alias = %placement.database_alias,
                placement_status = %placement.status,
                routing_decision = "readable_execution_route",
                "resolved readable execution route"
            );
            routed.push(ExecutionRoute { placement, pool });
        }

        Ok(routed)
    }

    pub(crate) async fn invalidate_execution_placement(&self, run_id: Uuid, run_shard: i16) {
        self.shard_placement_cache
            .invalidate(&ShardPlacementKey { run_id, run_shard })
            .await;
    }

    async fn select_control_shard_placement(
        &self,
        run_id: Uuid,
        run_shard: i16,
    ) -> anyhow::Result<ShardPlacement> {
        let db = self.control().await?;
        shard_placements::select_shard_placement(db, run_id, run_shard)
            .await?
            .ok_or_else(|| ExecutionRouteError::MissingShardPlacement { run_id, run_shard }.into())
    }

    pub(crate) async fn validate_placement_config(&self) -> anyhow::Result<()> {
        let db = self.control().await?;
        let placements = database_placements::list_serviceable_database_placements(db).await?;

        if placements.is_empty() {
            anyhow::bail!("database_placements has no active or draining placements");
        }

        for placement in &placements {
            self.resolve_database_url_env(&placement.database_url_env)?;
        }

        let control_placements = placements
            .iter()
            .filter(|placement| {
                placement.status == DATABASE_PLACEMENT_STATUS_ACTIVE
                    && placement.is_control_capable()
            })
            .collect::<Vec<_>>();

        let [active_control] = control_placements.as_slice() else {
            anyhow::bail!(
                "expected exactly one active control-capable database placement, found {}",
                control_placements.len()
            );
        };

        if active_control.alias != self.placement_config.control_database_alias {
            anyhow::bail!(
                "VIGILO_CONTROL_DATABASE_ALIAS={} does not match active control placement {}",
                self.placement_config.control_database_alias,
                active_control.alias
            );
        }

        let placements_by_alias = placements
            .iter()
            .map(|placement| (placement.alias.as_str(), placement))
            .collect::<HashMap<_, _>>();

        validate_shard_capable_alias(
            &placements_by_alias,
            &self.placement_config.default_shard_database_alias,
            "VIGILO_DEFAULT_SHARD_DATABASE_ALIAS",
        )?;

        let disabled_routes =
            database_placements::count_shard_placements_on_disabled_databases(db).await?;
        if disabled_routes > 0 {
            anyhow::bail!(
                "{} shard placement row(s) route to disabled database placements",
                disabled_routes
            );
        }

        Ok(())
    }

    fn resolve_database_url_env(&self, database_url_env: &str) -> anyhow::Result<String> {
        if database_url_env == DEFAULT_DATABASE_URL_ENV {
            return Ok(self.uri.clone());
        }

        std::env::var(database_url_env).map_err(|_| {
            anyhow::anyhow!(
                "serviceable database placement references unset env var {}",
                database_url_env
            )
        })
    }

    pub(crate) fn database_url_env_is_resolved(&self, database_url_env: &str) -> bool {
        database_url_env == DEFAULT_DATABASE_URL_ENV || std::env::var_os(database_url_env).is_some()
    }

    #[allow(dead_code)]
    async fn placement_pools(&self) -> anyhow::Result<&PlacementPools> {
        self.placement_pools
            .get_or_try_init(|| async {
                let db = self.control().await?;
                let placements =
                    database_placements::list_serviceable_database_placements(db).await?;

                if placements.is_empty() {
                    anyhow::bail!("database_placements has no active or draining placements");
                }

                let mut pools_by_alias = HashMap::with_capacity(placements.len());

                for placement in placements {
                    let database_url =
                        self.resolve_database_url_env(&placement.database_url_env)?;
                    let alias = placement.alias.clone();
                    pools_by_alias.insert(
                        alias,
                        PlacementPool {
                            database_url_env: placement.database_url_env,
                            database_url,
                            pool: OnceCell::new(),
                        },
                    );
                }

                Ok(PlacementPools { pools_by_alias })
            })
            .await
    }

    async fn require_active_placement(&self, alias: &str) -> anyhow::Result<DatabasePlacement> {
        let db = self.control().await?;
        let placement = database_placements::select_database_placement(db, alias)
            .await?
            .ok_or_else(|| anyhow::anyhow!("database placement alias {} was not found", alias))?;

        if placement.status != DATABASE_PLACEMENT_STATUS_ACTIVE {
            anyhow::bail!(
                "database placement alias {} has status {}, which cannot receive new shard ownership",
                alias,
                placement.status
            );
        }

        Ok(placement)
    }

    async fn require_serviceable_placement(
        &self,
        alias: &str,
    ) -> anyhow::Result<DatabasePlacement> {
        let db = self.control().await?;
        let placement = database_placements::select_database_placement(db, alias)
            .await?
            .ok_or_else(|| anyhow::anyhow!("database placement alias {} was not found", alias))?;

        if !placement.can_serve_owned_shards() {
            anyhow::bail!(
                "database placement alias {} has status {}, which cannot serve existing shard ownership",
                alias,
                placement.status
            );
        }

        Ok(placement)
    }

    #[allow(dead_code)]
    async fn validate_shard_placement_alias(
        &self,
        placement: &ShardPlacement,
    ) -> anyhow::Result<()> {
        let database_placement = self
            .require_serviceable_placement(&placement.database_alias)
            .await?;

        require_shard_capable_placement(&database_placement)
    }
}

async fn select_shard_placement_on_connection(
    tx: &mut Transaction<'_, Postgres>,
    run_id: Uuid,
    run_shard: i16,
) -> anyhow::Result<ShardPlacement> {
    sqlx::query_as::<_, ShardPlacement>(
        r#"
        SELECT run_id, run_shard, database_alias, status, move_target_database_alias,
               route_version, created_at, updated_at
        FROM shard_placements
        WHERE run_id = $1::uuid
          AND run_shard = $2
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ExecutionRouteError::MissingShardPlacement { run_id, run_shard }.into())
}

pub(crate) fn new_shard_placement_cache() -> Cache<ShardPlacementKey, ShardPlacement> {
    Cache::builder()
        .time_to_live(SHARD_PLACEMENT_CACHE_TTL)
        .max_capacity(SHARD_PLACEMENT_CACHE_CAPACITY)
        .build()
}

pub(crate) struct PlacementPools {
    #[allow(dead_code)]
    pools_by_alias: HashMap<String, PlacementPool>,
}

pub(crate) struct PlacementPool {
    database_url_env: String,
    #[allow(dead_code)]
    database_url: String,
    #[allow(dead_code)]
    pool: OnceCell<PgPool>,
}

impl PlacementPool {
    #[allow(dead_code)]
    async fn get(&self, alias: &str, max_connections: u32) -> anyhow::Result<&PgPool> {
        self.pool
            .get_or_try_init(|| async {
                debug!(database_alias = %alias, "initializing postgres placement connection");

                PgPoolOptions::new()
                    .max_connections(max_connections)
                    .connect(&self.database_url)
                    .await
                    .map_err(|error| {
                        let message =
                            format!("database placement {alias} connection failed: {error}");
                        anyhow::Error::new(error).context(message)
                    })
            })
            .await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ShardPlacementKey {
    run_id: Uuid,
    run_shard: i16,
}

fn normalize_alias(alias: String, label: &str) -> anyhow::Result<String> {
    let alias = alias.trim().to_string();
    if alias.is_empty() {
        anyhow::bail!("{} must not be empty", label);
    }

    Ok(alias)
}

fn validate_shard_capable_alias(
    placements_by_alias: &HashMap<&str, &crate::models::database_placement::DatabasePlacement>,
    alias: &str,
    config_name: &str,
) -> anyhow::Result<()> {
    let Some(placement) = placements_by_alias.get(alias) else {
        anyhow::bail!(
            "{}={} does not match an active or draining database placement",
            config_name,
            alias
        );
    };

    if !placement.accepts_new_shards() {
        anyhow::bail!(
            "{}={} points to placement status {}, which cannot receive new shard ownership",
            config_name,
            alias,
            placement.status
        );
    }

    if !placement.is_shard_capable() {
        anyhow::bail!(
            "{}={} points to placement role {}, which is not shard-capable",
            config_name,
            alias,
            placement.role
        );
    }

    Ok(())
}

fn require_shard_capable_placement(placement: &DatabasePlacement) -> anyhow::Result<()> {
    if !placement.is_shard_capable() {
        anyhow::bail!(
            "database placement alias {} has role {}, which is not shard-capable",
            placement.alias,
            placement.role
        );
    }

    Ok(())
}

#[allow(dead_code)]
fn validate_run_shard(run_shard: i16) -> anyhow::Result<()> {
    if !(0..crate::models::run_chunk::RUN_SHARD_COUNT).contains(&run_shard) {
        anyhow::bail!(
            "run_shard {} is outside the supported range 0..{}",
            run_shard,
            crate::models::run_chunk::RUN_SHARD_COUNT
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::{
        PgPool,
        postgres::PgPoolOptions,
    };
    use uuid::Uuid;

    use super::*;
    use crate::models::{
        database_placement::DEFAULT_DATABASE_ALIAS,
        shard_placement::{
            SHARD_PLACEMENT_STATUS_ACTIVE,
            SHARD_PLACEMENT_STATUS_MOVING,
        },
    };

    #[test]
    fn default_config_uses_primary_aliases() {
        let config = PlacementConfig::default_single_database();

        assert_eq!(config.control_database_alias, DEFAULT_DATABASE_ALIAS);
        assert_eq!(config.default_shard_database_alias, DEFAULT_DATABASE_ALIAS);
        assert_eq!(
            config.shard_assignment_policy,
            ShardAssignmentPolicy::SingleDefault
        );
    }

    #[test]
    fn config_normalizes_aliases() {
        let config = PlacementConfig::new(
            " primary ".to_string(),
            " exec_a ".to_string(),
            " spread-active ".to_string(),
        )
        .unwrap();

        assert_eq!(config.control_database_alias, "primary");
        assert_eq!(config.default_shard_database_alias, "exec_a");
        assert_eq!(
            config.shard_assignment_policy,
            ShardAssignmentPolicy::SpreadActive
        );
    }

    #[test]
    fn config_rejects_empty_control_alias() {
        let error = PlacementConfig::new(
            " ".to_string(),
            "primary".to_string(),
            "single-default".to_string(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("control alias must not be empty")
        );
    }

    #[test]
    fn config_rejects_empty_default_shard_alias() {
        let error = PlacementConfig::new(
            "primary".to_string(),
            " ".to_string(),
            "single-default".to_string(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("default shard alias must not be empty")
        );
    }

    #[test]
    fn config_rejects_unknown_shard_assignment_policy() {
        let error = PlacementConfig::new(
            "primary".to_string(),
            "primary".to_string(),
            "unknown".to_string(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unsupported shard assignment policy")
        );
    }

    #[test]
    fn validates_run_shard_range() {
        assert!(validate_run_shard(0).is_ok());
        assert!(validate_run_shard(127).is_ok());

        let low_error = validate_run_shard(-1).unwrap_err();
        assert!(
            low_error
                .to_string()
                .contains("outside the supported range")
        );

        let high_error = validate_run_shard(128).unwrap_err();
        assert!(
            high_error
                .to_string()
                .contains("outside the supported range")
        );
    }

    #[tokio::test]
    async fn database_context_get_returns_one_router_without_opening_pool() {
        let context = Context {
            config: Config {
                uri: "postgres://lazy-control-pool".to_string(),
                max_connections: 5,
                placement_config: PlacementConfig::default_single_database(),
            },
            cell: OnceCell::new(),
        };

        let first = context.get().await.unwrap() as *const DatabaseRouter;
        let second = context.get().await.unwrap() as *const DatabaseRouter;

        assert_eq!(first, second);
        assert!(context.cell.get().unwrap().control_pool.get().is_none());
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn execution_routes_active_primary_placement_to_control_pool(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);
        let run_id = Uuid::now_v7();

        insert_shard_placement(
            database_router.control().await.unwrap(),
            run_id,
            42,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;

        let control = database_router.control().await.unwrap() as *const PgPool;
        let routed = database_router.execution(run_id, 42).await.unwrap() as *const PgPool;
        assert_eq!(control, routed);

        let placement = database_router
            .execution_placement(run_id, 42)
            .await
            .unwrap();
        assert_eq!(placement.database_alias, DEFAULT_DATABASE_ALIAS);
        assert_eq!(placement.status, SHARD_PLACEMENT_STATUS_ACTIVE);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn draining_placement_serves_owned_routes_but_rejects_new_ownership(pool: PgPool) {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'draining')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let database_url = isolated_database_url(&pool).await;
        let database_router = database_router_with_control_pool_and_uri(pool, database_url);
        let run_id = Uuid::now_v7();
        insert_shard_placement(
            database_router.control().await.unwrap(),
            run_id,
            42,
            "shard_001",
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;

        database_router.execution(run_id, 42).await.unwrap();

        let error = database_router
            .execution_target_database("shard_001")
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot receive new shard ownership")
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn execution_rejects_moving_placement(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);
        let run_id = Uuid::now_v7();

        insert_shard_placement(
            database_router.control().await.unwrap(),
            run_id,
            7,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;
        let draining =
            mark_test_shard_placement_draining(database_router.control().await.unwrap(), run_id, 7)
                .await;
        mark_test_shard_placement_moving(database_router.control().await.unwrap(), &draining).await;

        let error = database_router.execution(run_id, 7).await.unwrap_err();
        assert!(error.to_string().contains("not dispatchable"));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn execution_placement_refreshes_stale_cached_route_version(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);
        let run_id = Uuid::now_v7();

        insert_shard_placement(
            database_router.control().await.unwrap(),
            run_id,
            7,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;

        let initial = database_router
            .execution_placement(run_id, 7)
            .await
            .unwrap();
        assert_eq!(initial.status, SHARD_PLACEMENT_STATUS_ACTIVE);
        assert_eq!(initial.route_version, 1);

        let draining =
            mark_test_shard_placement_draining(database_router.control().await.unwrap(), run_id, 7)
                .await;
        mark_test_shard_placement_moving(database_router.control().await.unwrap(), &draining).await;

        let refreshed = database_router
            .execution_placement(run_id, 7)
            .await
            .unwrap();
        assert_eq!(refreshed.status, SHARD_PLACEMENT_STATUS_MOVING);
        assert_eq!(refreshed.route_version, 4);

        let error = database_router.execution(run_id, 7).await.unwrap_err();
        assert!(error.to_string().contains("not dispatchable"));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn stale_execution_route_cannot_claim_after_move_begins(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);
        let run_id = Uuid::now_v7();
        let chunk_id =
            seed_claimable_chunk(database_router.control().await.unwrap(), run_id, 7).await;
        insert_shard_placement(
            database_router.control().await.unwrap(),
            run_id,
            7,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;
        let stale_route = database_router.execution_route(run_id, 7).await.unwrap();

        let draining =
            mark_test_shard_placement_draining(database_router.control().await.unwrap(), run_id, 7)
                .await;
        mark_test_shard_placement_moving(database_router.control().await.unwrap(), &draining).await;

        let error = crate::db::workflows::chunk_processing::claim_routed_chunk_for_processing(
            &database_router,
            &stale_route,
            run_id,
            7,
            chunk_id,
            60,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<ExecutionRouteError>(),
            Some(ExecutionRouteError::StaleExecutionRoute { .. })
        ));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn routed_cancellation_waits_for_shard_move_admission(pool: PgPool) {
        let database_name = sqlx::query_scalar::<_, String>("SELECT current_database()")
            .fetch_one(&pool)
            .await
            .unwrap();
        let mut database_url = url::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
        database_url.set_path(&database_name);
        let runtime_pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url.as_str())
            .await
            .unwrap();
        let database_router = database_router_with_control_pool(runtime_pool);
        let run_id = Uuid::now_v7();
        seed_claimable_chunk(database_router.control().await.unwrap(), run_id, 7).await;
        insert_shard_placement(
            database_router.control().await.unwrap(),
            run_id,
            7,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;

        let mut move_tx = database_router
            .control()
            .await
            .unwrap()
            .begin()
            .await
            .unwrap();
        crate::db::shard_write_fence::lock_exclusive(&mut move_tx, run_id, 7)
            .await
            .unwrap();
        let cancellation =
            crate::db::workflows::run_cancel::cancel_run_routed(&database_router, run_id);
        tokio::pin!(cancellation);

        assert!(
            tokio::time::timeout(Duration::from_millis(500), cancellation.as_mut())
                .await
                .is_err(),
            "cancellation must not mutate source rows while movement holds exclusive admission"
        );

        move_tx.rollback().await.unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(10), cancellation.as_mut())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(outcome.cancelled);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn cleanup_admission_allows_draining_but_rejects_moving(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);
        let run_id = Uuid::now_v7();
        seed_claimable_chunk(database_router.control().await.unwrap(), run_id, 7).await;
        insert_shard_placement(
            database_router.control().await.unwrap(),
            run_id,
            7,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;
        let draining =
            mark_test_shard_placement_draining(database_router.control().await.unwrap(), run_id, 7)
                .await;

        let draining_routes = database_router
            .execution_read_routes_with_fences_for_run(run_id)
            .await
            .unwrap();
        database_router
            .begin_execution_cleanup_admission(&draining_routes)
            .await
            .unwrap()
            .rollback()
            .await
            .unwrap();

        mark_test_shard_placement_moving(database_router.control().await.unwrap(), &draining).await;
        let moving_routes = database_router
            .execution_read_routes_with_fences_for_run(run_id)
            .await
            .unwrap();
        let error = database_router
            .begin_execution_cleanup_admission(&moving_routes)
            .await
            .unwrap_err();

        assert!(matches!(
            error.downcast_ref::<ExecutionRouteError>(),
            Some(ExecutionRouteError::NonDispatchableShardPlacement { status, .. })
                if status == SHARD_PLACEMENT_STATUS_MOVING
        ));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn execution_placement_requires_stored_shard_placement(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);
        let run_id = Uuid::now_v7();

        let error = database_router
            .execution_placement(run_id, 3)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("missing shard placement"));
        assert!(
            error
                .to_string()
                .contains("run creation must insert shard_placements rows")
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn execution_routes_for_run_returns_routed_primary_pools(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);
        let run_id = Uuid::now_v7();

        insert_shard_placement(
            database_router.control().await.unwrap(),
            run_id,
            2,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;
        insert_shard_placement(
            database_router.control().await.unwrap(),
            run_id,
            9,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;

        let routed = database_router
            .execution_routes_for_run(run_id)
            .await
            .unwrap();

        assert_eq!(routed.len(), 2);
        assert_eq!(routed[0].0, 2);
        assert_eq!(routed[0].1, DEFAULT_DATABASE_ALIAS);
        assert_eq!(routed[1].0, 9);
        assert_eq!(routed[1].1, DEFAULT_DATABASE_ALIAS);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn execution_read_routes_for_run_include_moving_placements(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);
        let run_id = Uuid::now_v7();

        insert_shard_placement(
            database_router.control().await.unwrap(),
            run_id,
            7,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;
        let draining =
            mark_test_shard_placement_draining(database_router.control().await.unwrap(), run_id, 7)
                .await;
        mark_test_shard_placement_moving(database_router.control().await.unwrap(), &draining).await;

        let dispatch_error = database_router
            .execution_routes_for_run(run_id)
            .await
            .unwrap_err();
        assert!(dispatch_error.to_string().contains("not dispatchable"));

        let readable = database_router
            .execution_read_routes_for_run(run_id)
            .await
            .unwrap();
        assert_eq!(readable.len(), 1);
        assert_eq!(readable[0].0, 7);
        assert_eq!(readable[0].1, DEFAULT_DATABASE_ALIAS);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn placement_rejects_alias_disabled_after_pool_initialization(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);

        database_router
            .placement(DEFAULT_DATABASE_ALIAS)
            .await
            .unwrap();
        assert!(database_router.placement_pools.get().is_some());

        sqlx::query(
            r#"
            UPDATE database_placements
            SET status = 'disabled', updated_at = now()
            WHERE alias = $1
            "#,
        )
        .bind(DEFAULT_DATABASE_ALIAS)
        .execute(database_router.control().await.unwrap())
        .await
        .unwrap();

        let error = database_router
            .placement(DEFAULT_DATABASE_ALIAS)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("status disabled"));
        assert!(database_router.placement_pools.get().is_some());
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn execution_rejects_live_non_shard_role(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);
        let run_id = Uuid::now_v7();

        insert_shard_placement(
            database_router.control().await.unwrap(),
            run_id,
            7,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;
        database_router.execution(run_id, 7).await.unwrap();

        sqlx::query(
            r#"
            UPDATE database_placements
            SET role = 'control', updated_at = now()
            WHERE alias = $1
            "#,
        )
        .bind(DEFAULT_DATABASE_ALIAS)
        .execute(database_router.control().await.unwrap())
        .await
        .unwrap();

        database_router
            .placement(DEFAULT_DATABASE_ALIAS)
            .await
            .unwrap();
        let error = database_router.execution(run_id, 7).await.unwrap_err();
        assert!(error.to_string().contains("role control"));
        assert!(error.to_string().contains("not shard-capable"));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn placement_requires_restart_for_alias_added_after_pool_initialization(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);
        database_router
            .placement(DEFAULT_DATABASE_ALIAS)
            .await
            .unwrap();

        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_late', 'VIGILO_TEST_LATE_SHARD_URL', 'shard', 'active')
            "#,
        )
        .execute(database_router.control().await.unwrap())
        .await
        .unwrap();

        let error = database_router.placement("shard_late").await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no process-local connection configuration")
        );
        assert!(error.to_string().contains("restart Vigilo"));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn placement_requires_restart_after_connection_metadata_changes(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);
        database_router
            .placement(DEFAULT_DATABASE_ALIAS)
            .await
            .unwrap();

        sqlx::query(
            r#"
            UPDATE database_placements
            SET database_url_env = 'VIGILO_TEST_REPLACED_DATABASE_URL',
                updated_at = now()
            WHERE alias = $1
            "#,
        )
        .bind(DEFAULT_DATABASE_ALIAS)
        .execute(database_router.control().await.unwrap())
        .await
        .unwrap();

        let error = database_router
            .placement(DEFAULT_DATABASE_ALIAS)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("changed database_url_env"));
        assert!(error.to_string().contains("restart Vigilo"));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn active_placement_with_missing_env_var_fails_clearly(pool: PgPool) {
        let database_router = database_router_with_control_pool(pool);

        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'VIGILO_TEST_MISSING_SHARD_URL', 'shard', 'active')
            "#,
        )
        .execute(database_router.control().await.unwrap())
        .await
        .unwrap();

        let error = database_router.placement("shard_001").await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("serviceable database placement references unset env var")
        );
    }

    fn database_router_with_control_pool(pool: PgPool) -> DatabaseRouter {
        database_router_with_control_pool_and_uri(
            pool,
            "postgres://injected-control-pool".to_string(),
        )
    }

    fn database_router_with_control_pool_and_uri(pool: PgPool, uri: String) -> DatabaseRouter {
        let database_router = DatabaseRouter {
            uri,
            max_connections: 5,
            placement_config: PlacementConfig::default_single_database(),
            control_pool: OnceCell::new(),
            placement_pools: OnceCell::new(),
            shard_placement_cache: new_shard_placement_cache(),
        };
        assert!(database_router.control_pool.set(pool).is_ok());
        database_router
    }

    async fn isolated_database_url(pool: &PgPool) -> String {
        let database_name = sqlx::query_scalar::<_, String>("SELECT current_database()")
            .fetch_one(pool)
            .await
            .unwrap();
        let mut database_url =
            url::Url::parse(&std::env::var(DEFAULT_DATABASE_URL_ENV).unwrap()).unwrap();
        database_url.set_path(&database_name);
        database_url.to_string()
    }

    async fn insert_shard_placement(
        db: &PgPool,
        run_id: Uuid,
        run_shard: i16,
        database_alias: &str,
        status: &str,
    ) {
        sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(run_id)
        .bind(run_shard)
        .bind(database_alias)
        .bind(status)
        .execute(db)
        .await
        .unwrap();
    }

    async fn mark_test_shard_placement_draining(
        db: &PgPool,
        run_id: Uuid,
        run_shard: i16,
    ) -> ShardPlacement {
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'DATABASE_URL', 'shard', 'active')
            ON CONFLICT (alias) DO NOTHING
            "#,
        )
        .execute(db)
        .await
        .unwrap();

        let current = shard_placements::select_shard_placement(db, run_id, run_shard)
            .await
            .unwrap()
            .unwrap();
        let copying = shard_placements::mark_shard_placement_copying(
            db,
            run_id,
            run_shard,
            &current.database_alias,
            current.route_version,
            "shard_001",
        )
        .await
        .unwrap()
        .unwrap();
        shard_placements::mark_shard_placement_draining(
            db,
            run_id,
            run_shard,
            &copying.database_alias,
            copying.route_version,
            copying.move_target_database_alias.as_deref().unwrap(),
        )
        .await
        .unwrap()
        .unwrap()
    }

    async fn mark_test_shard_placement_moving(
        db: &PgPool,
        draining: &ShardPlacement,
    ) -> ShardPlacement {
        shard_placements::mark_shard_placement_moving(
            db,
            draining.run_id,
            draining.run_shard,
            &draining.database_alias,
            draining.route_version,
            draining.move_target_database_alias.as_deref().unwrap(),
        )
        .await
        .unwrap()
        .unwrap()
    }

    async fn seed_claimable_chunk(db: &PgPool, run_id: Uuid, run_shard: i16) -> Uuid {
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        let chunk_id = Uuid::now_v7();

        sqlx::query(
            r#"
            INSERT INTO dataset_versions (dataset_version_id, dataset_id, dataset_version)
            VALUES ($1::uuid, $2::uuid, 'test')
            "#,
        )
        .bind(dataset_version_id)
        .bind(dataset_id)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO runs (
                id, run_key,
                dataset_id, dataset_version_id, dataset_version,
                evaluation_profile_id, evaluation_profile_version,
                profile_version_id, profile_hash,
                aggregation_policy_id, aggregation_policy_version, aggregation_policy_hash,
                agent_provider, agent_name,
                prompt_config_id, prompt_config_version,
                status, expected_execution_count
            )
            VALUES (
                $1::uuid, $2,
                $3::uuid, $4::uuid, 'test',
                'profile', '1', 'profile:1', 'profile-hash',
                'policy', '1', 'policy-hash',
                'test', 'agent', 'prompt', '1',
                'running'::run_status, 1
            )
            "#,
        )
        .bind(run_id)
        .bind(format!("run-{run_id}"))
        .bind(dataset_id)
        .bind(dataset_version_id)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO run_chunks (
                id, run_id, run_shard, dataset_version_id,
                profile_group_id, ordinal_start, ordinal_end, status
            )
            VALUES ($1::uuid, $2::uuid, $3, $4::uuid, 'default', 0, 1, 'pending')
            "#,
        )
        .bind(chunk_id)
        .bind(run_id)
        .bind(run_shard)
        .bind(dataset_version_id)
        .execute(db)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO run_snapshots (
                run_id, run_shard, run_key,
                dataset_id, dataset_version_id, dataset_version,
                evaluation_profile_id, evaluation_profile_version,
                profile_version_id, profile_hash,
                aggregation_policy_id, aggregation_policy_version, aggregation_policy_hash,
                agent_provider, agent_name,
                prompt_config_id, prompt_config_version,
                expected_execution_count
            )
            VALUES (
                $1::uuid, $2, $3,
                $4::uuid, $5::uuid, 'test',
                'profile', '1', 'profile:1', 'profile-hash',
                'policy', '1', 'policy-hash',
                'test', 'agent', 'prompt', '1', 1
            )
            "#,
        )
        .bind(run_id)
        .bind(run_shard)
        .bind(format!("run-{run_id}"))
        .bind(dataset_id)
        .bind(dataset_version_id)
        .execute(db)
        .await
        .unwrap();

        chunk_id
    }
}
