//! Lazy PostgreSQL pool and placement configuration context.
//!
//! Commands call this module through [`crate::context::Context::db`] when they
//! first need database access. This context initializes the database service
//! once, then the service lazily initializes its control pool, placement
//! catalog, and per-placement pools. The placement catalog lives in the
//! database; environment variables only provide secret connection values.
//!
//! Keep topology choices behind DB workflow boundaries. Thin command dispatch,
//! generic runtime code, and arbitrary domain callers should prefer workflows
//! over directly choosing control storage or execution storage.

use std::{
    collections::HashMap,
    time::Duration,
};

use moka::future::Cache;
use sqlx::{
    PgPool,
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
            DEFAULT_DATABASE_URL_ENV,
            DatabasePlacement,
        },
        shard_placement::ShardPlacement,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlacementConfig {
    pub(crate) control_database_alias: String,
    pub(crate) default_shard_database_alias: String,
}

impl PlacementConfig {
    pub(crate) fn new(
        control_database_alias: String,
        default_shard_database_alias: String,
    ) -> anyhow::Result<Self> {
        let control_database_alias = normalize_alias(control_database_alias, "control alias")?;
        let default_shard_database_alias =
            normalize_alias(default_shard_database_alias, "default shard alias")?;

        Ok(Self {
            control_database_alias,
            default_shard_database_alias,
        })
    }

    #[cfg(test)]
    pub(crate) fn default_single_database() -> Self {
        Self {
            control_database_alias: crate::models::database_placement::DEFAULT_DATABASE_ALIAS
                .to_string(),
            default_shard_database_alias: crate::models::database_placement::DEFAULT_DATABASE_ALIAS
                .to_string(),
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
    pub(crate) cell: OnceCell<Db>,
}

impl Context {
    pub(crate) async fn get(&self) -> anyhow::Result<&Db> {
        self.cell
            .get_or_try_init(|| async { Ok::<Db, anyhow::Error>(Db::new(self.config.clone())) })
            .await
    }
}

pub(crate) struct Db {
    pub(crate) uri: String,
    pub(crate) max_connections: u32,
    pub(crate) placement_config: PlacementConfig,
    pub(crate) cell: OnceCell<PgPool>,
    #[allow(dead_code)]
    pub(crate) placement_catalog: OnceCell<PlacementCatalog>,
    #[allow(dead_code)]
    pub(crate) shard_placement_cache: Cache<ShardPlacementKey, ShardPlacement>,
}

impl Db {
    fn new(config: Config) -> Self {
        Self {
            uri: config.uri,
            max_connections: config.max_connections,
            placement_config: config.placement_config,
            cell: OnceCell::new(),
            placement_catalog: OnceCell::new(),
            shard_placement_cache: new_shard_placement_cache(),
        }
    }

    /// Returns the control-plane database pool.
    ///
    /// The pool is initialized on first use, not when the database service is
    /// retrieved from [`crate::context::Context::db`].
    pub async fn control(&self) -> anyhow::Result<&PgPool> {
        self.cell
            .get_or_try_init(|| async {
                debug!("initializing postgres database connection");

                PgPoolOptions::new()
                    .max_connections(self.max_connections)
                    .connect(&self.uri)
                    .await
                    .map_err(|e| anyhow::anyhow!("database connection failed: {}", e))
            })
            .await
    }

    pub(crate) fn default_execution_database_alias(&self) -> &str {
        &self.placement_config.default_shard_database_alias
    }

    pub(crate) async fn active_execution_database_aliases(&self) -> anyhow::Result<Vec<String>> {
        let db = self.control().await?;
        database_placements::list_active_shard_database_aliases(db).await
    }

    pub(crate) async fn active_outbox_database_aliases(&self) -> anyhow::Result<Vec<String>> {
        let db = self.control().await?;
        database_placements::list_active_database_aliases(db).await
    }

    /// Returns a pool for an explicit placement alias.
    ///
    /// This is an infrastructure hook for router/admin workflows. Most callers
    /// should use a domain workflow or [`Self::execution`] instead of naming a
    /// placement directly.
    #[allow(dead_code)]
    pub async fn placement(&self, alias: &str) -> anyhow::Result<&PgPool> {
        let alias = normalize_alias(alias.to_string(), "database alias")?;
        let catalog = self.placement_catalog().await?;
        catalog.require_active_alias(&alias)?;

        if alias == self.placement_config.control_database_alias {
            return self.control().await;
        }

        let Some(pool) = catalog.pools_by_alias.get(&alias) else {
            anyhow::bail!("database placement alias {} is not configured", alias);
        };

        pool.get(&alias, self.max_connections).await
    }

    /// Resolves the stored execution placement for a run shard.
    #[allow(dead_code)]
    pub async fn execution_placement(
        &self,
        run_id: Uuid,
        run_shard: i16,
    ) -> anyhow::Result<ShardPlacement> {
        validate_run_shard(run_shard)?;

        let key = ShardPlacementKey { run_id, run_shard };
        if let Some(placement) = self.shard_placement_cache.get(&key).await {
            self.validate_shard_placement_alias(&placement).await?;
            return Ok(placement);
        }

        let db = self.control().await?;
        let Some(placement) =
            shard_placements::select_shard_placement(db, run_id, run_shard).await?
        else {
            return Err(ExecutionRouteError::MissingShardPlacement { run_id, run_shard }.into());
        };

        self.validate_shard_placement_alias(&placement).await?;
        self.shard_placement_cache
            .insert(key, placement.clone())
            .await;

        Ok(placement)
    }

    /// Returns the execution-owned database pool for a run shard.
    ///
    /// Routing is based on persisted `shard_placements` data. Callers that do
    /// not own execution storage should use a workflow that hides this choice.
    #[allow(dead_code)]
    pub async fn execution(&self, run_id: Uuid, run_shard: i16) -> anyhow::Result<&PgPool> {
        let placement = self.execution_placement(run_id, run_shard).await?;

        if !placement.is_dispatchable() {
            return Err(ExecutionRouteError::NonDispatchableShardPlacement {
                run_id,
                run_shard,
                status: placement.status,
            }
            .into());
        }

        self.placement(&placement.database_alias).await
    }

    /// Returns all dispatchable execution routes for a run.
    #[allow(dead_code)]
    pub async fn execution_routes_for_run(
        &self,
        run_id: Uuid,
    ) -> anyhow::Result<Vec<(i16, String, PgPool)>> {
        let db = self.control().await?;
        let placements = shard_placements::list_shard_placements_for_run(db, run_id).await?;
        let mut routed = Vec::with_capacity(placements.len());

        for placement in placements {
            self.validate_shard_placement_alias(&placement).await?;
            if !placement.is_dispatchable() {
                anyhow::bail!(
                    "shard placement for run {} shard {} has status {}, which is not dispatchable",
                    run_id,
                    placement.run_shard,
                    placement.status
                );
            }

            let pool = self.placement(&placement.database_alias).await?.clone();
            routed.push((placement.run_shard, placement.database_alias, pool));
        }

        Ok(routed)
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
        let db = self.control().await?;
        let placements = shard_placements::list_shard_placements_for_run(db, run_id).await?;
        let mut routed = Vec::with_capacity(placements.len());

        for placement in placements {
            self.validate_shard_placement_alias(&placement).await?;
            let pool = self.placement(&placement.database_alias).await?.clone();
            routed.push((placement.run_shard, placement.database_alias, pool));
        }

        Ok(routed)
    }

    pub(crate) async fn validate_placement_config(&self) -> anyhow::Result<()> {
        let db = self.control().await?;
        let placements = database_placements::list_active_database_placements(db).await?;

        if placements.is_empty() {
            anyhow::bail!("database_placements has no active placements");
        }

        for placement in &placements {
            self.resolve_database_url_env(&placement.database_url_env)?;
        }

        let control_placements = placements
            .iter()
            .filter(|placement| placement.is_control_capable())
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
                "active database placement references unset env var {}",
                database_url_env
            )
        })
    }

    #[allow(dead_code)]
    async fn placement_catalog(&self) -> anyhow::Result<&PlacementCatalog> {
        self.placement_catalog
            .get_or_try_init(|| async {
                let db = self.control().await?;
                let placements = database_placements::list_active_database_placements(db).await?;

                if placements.is_empty() {
                    anyhow::bail!("database_placements has no active placements");
                }

                let mut placements_by_alias = HashMap::with_capacity(placements.len());
                let mut pools_by_alias = HashMap::with_capacity(placements.len());

                for placement in placements {
                    let database_url =
                        self.resolve_database_url_env(&placement.database_url_env)?;
                    let alias = placement.alias.clone();
                    pools_by_alias.insert(
                        alias.clone(),
                        PlacementPool {
                            database_url,
                            cell: OnceCell::new(),
                        },
                    );
                    placements_by_alias.insert(alias, placement);
                }

                Ok(PlacementCatalog {
                    placements_by_alias,
                    pools_by_alias,
                })
            })
            .await
    }

    #[allow(dead_code)]
    async fn validate_shard_placement_alias(
        &self,
        placement: &ShardPlacement,
    ) -> anyhow::Result<()> {
        let catalog = self.placement_catalog().await?;
        catalog.require_shard_capable_alias(&placement.database_alias)
    }
}

pub(crate) fn new_shard_placement_cache() -> Cache<ShardPlacementKey, ShardPlacement> {
    Cache::builder()
        .time_to_live(SHARD_PLACEMENT_CACHE_TTL)
        .max_capacity(SHARD_PLACEMENT_CACHE_CAPACITY)
        .build()
}

pub(crate) struct PlacementCatalog {
    #[allow(dead_code)]
    placements_by_alias: HashMap<String, DatabasePlacement>,
    #[allow(dead_code)]
    pools_by_alias: HashMap<String, PlacementPool>,
}

impl PlacementCatalog {
    #[allow(dead_code)]
    fn require_active_alias(&self, alias: &str) -> anyhow::Result<&DatabasePlacement> {
        self.placements_by_alias
            .get(alias)
            .ok_or_else(|| anyhow::anyhow!("database placement alias {} is not active", alias))
    }

    #[allow(dead_code)]
    fn require_shard_capable_alias(&self, alias: &str) -> anyhow::Result<()> {
        let placement = self.require_active_alias(alias)?;

        if !placement.is_shard_capable() {
            anyhow::bail!(
                "database placement alias {} has role {}, which is not shard-capable",
                alias,
                placement.role
            );
        }

        Ok(())
    }
}

pub(crate) struct PlacementPool {
    #[allow(dead_code)]
    database_url: String,
    #[allow(dead_code)]
    cell: OnceCell<PgPool>,
}

impl PlacementPool {
    #[allow(dead_code)]
    async fn get(&self, alias: &str, max_connections: u32) -> anyhow::Result<&PgPool> {
        self.cell
            .get_or_try_init(|| async {
                debug!(database_alias = %alias, "initializing postgres placement connection");

                PgPoolOptions::new()
                    .max_connections(max_connections)
                    .connect(&self.database_url)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("database placement {} connection failed: {}", alias, e)
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
            "{}={} does not match an active database placement",
            config_name,
            alias
        );
    };

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
    use sqlx::PgPool;
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
    }

    #[test]
    fn config_normalizes_aliases() {
        let config = PlacementConfig::new(" primary ".to_string(), " exec_a ".to_string()).unwrap();

        assert_eq!(config.control_database_alias, "primary");
        assert_eq!(config.default_shard_database_alias, "exec_a");
    }

    #[test]
    fn config_rejects_empty_control_alias() {
        let error = PlacementConfig::new(" ".to_string(), "primary".to_string()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("control alias must not be empty")
        );
    }

    #[test]
    fn config_rejects_empty_default_shard_alias() {
        let error = PlacementConfig::new("primary".to_string(), " ".to_string()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("default shard alias must not be empty")
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
    async fn database_context_get_returns_one_service_without_opening_pool() {
        let context = Context {
            config: Config {
                uri: "postgres://lazy-control-pool".to_string(),
                max_connections: 5,
                placement_config: PlacementConfig::default_single_database(),
            },
            cell: OnceCell::new(),
        };

        let first = context.get().await.unwrap() as *const Db;
        let second = context.get().await.unwrap() as *const Db;

        assert_eq!(first, second);
        assert!(context.cell.get().unwrap().cell.get().is_none());
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn execution_routes_active_primary_placement_to_control_pool(pool: PgPool) {
        let context = context_with_control_pool(pool);
        let run_id = Uuid::now_v7();

        insert_shard_placement(
            context.control().await.unwrap(),
            run_id,
            42,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;

        let control = context.control().await.unwrap() as *const PgPool;
        let routed = context.execution(run_id, 42).await.unwrap() as *const PgPool;
        assert_eq!(control, routed);

        let placement = context.execution_placement(run_id, 42).await.unwrap();
        assert_eq!(placement.database_alias, DEFAULT_DATABASE_ALIAS);
        assert_eq!(placement.status, SHARD_PLACEMENT_STATUS_ACTIVE);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn execution_rejects_moving_placement(pool: PgPool) {
        let context = context_with_control_pool(pool);
        let run_id = Uuid::now_v7();

        insert_shard_placement(
            context.control().await.unwrap(),
            run_id,
            7,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_MOVING,
        )
        .await;

        let error = context.execution(run_id, 7).await.unwrap_err();
        assert!(error.to_string().contains("not dispatchable"));
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn execution_placement_requires_stored_shard_placement(pool: PgPool) {
        let context = context_with_control_pool(pool);
        let run_id = Uuid::now_v7();

        let error = context.execution_placement(run_id, 3).await.unwrap_err();

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
        let context = context_with_control_pool(pool);
        let run_id = Uuid::now_v7();

        insert_shard_placement(
            context.control().await.unwrap(),
            run_id,
            2,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;
        insert_shard_placement(
            context.control().await.unwrap(),
            run_id,
            9,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_ACTIVE,
        )
        .await;

        let routed = context.execution_routes_for_run(run_id).await.unwrap();

        assert_eq!(routed.len(), 2);
        assert_eq!(routed[0].0, 2);
        assert_eq!(routed[0].1, DEFAULT_DATABASE_ALIAS);
        assert_eq!(routed[1].0, 9);
        assert_eq!(routed[1].1, DEFAULT_DATABASE_ALIAS);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn execution_read_routes_for_run_include_moving_placements(pool: PgPool) {
        let context = context_with_control_pool(pool);
        let run_id = Uuid::now_v7();

        insert_shard_placement(
            context.control().await.unwrap(),
            run_id,
            7,
            DEFAULT_DATABASE_ALIAS,
            SHARD_PLACEMENT_STATUS_MOVING,
        )
        .await;

        let dispatch_error = context.execution_routes_for_run(run_id).await.unwrap_err();
        assert!(dispatch_error.to_string().contains("not dispatchable"));

        let readable = context.execution_read_routes_for_run(run_id).await.unwrap();
        assert_eq!(readable.len(), 1);
        assert_eq!(readable[0].0, 7);
        assert_eq!(readable[0].1, DEFAULT_DATABASE_ALIAS);
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx router tests"]
    async fn active_placement_with_missing_env_var_fails_clearly(pool: PgPool) {
        let context = context_with_control_pool(pool);

        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('shard_001', 'VIGILO_TEST_MISSING_SHARD_URL', 'shard', 'active')
            "#,
        )
        .execute(context.control().await.unwrap())
        .await
        .unwrap();

        let error = context.placement("shard_001").await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("active database placement references unset env var")
        );
    }

    fn context_with_control_pool(pool: PgPool) -> Db {
        let context = Db {
            uri: "postgres://injected-control-pool".to_string(),
            max_connections: 5,
            placement_config: PlacementConfig::default_single_database(),
            cell: OnceCell::new(),
            placement_catalog: OnceCell::new(),
            shard_placement_cache: new_shard_placement_cache(),
        };
        assert!(context.cell.set(pool).is_ok());
        context
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
}
